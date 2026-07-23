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

// setup-followups plan, provider_fastmode_profile_persistence: pub(crate)
// so sync.rs can resolve the same real index a Msg-level dispatch would,
// instead of hand-rolling the visible_indices-empty-fallback logic
// (current_visible_indices' own reason for existing) a second time and
// risking it drifting out of sync with this one.
pub(crate) fn selected_real_index(model: &Model) -> usize {
    current_visible_indices(model)
        .get(model.selected_thread)
        .copied()
        .unwrap_or(model.selected_thread)
}

// setup-followups plan, archive_thread_backend_verify: pub(crate) so a
// real-backend test can build the exact row shape production actually
// produces, rather than hand-crafting a fixture that risks silently
// drifting from what this function really outputs.
pub(crate) fn visible_thread_row(
    model: &Model,
    real_index: usize,
) -> Option<crate::models::VisibleThreadItem> {
    let thread = model.threads.get(real_index)?;
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
        item: crate::ThreadItem {
            name: thread.display_name.clone().into(),
            status: status.into(),
            busy: matches!(
                thread.state,
                ThreadState::Loading | ThreadState::Cancelling
            ) && !thread.closed
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
            model: cached
                .as_ref()
                .map(|c| c.model.clone())
                .unwrap_or_default(),
            project_path: cached
                .as_ref()
                .map(|c| c.project_path.clone())
                .unwrap_or_default(),
            project_name: cached
                .as_ref()
                .map(|c| c.project_name.clone())
                .unwrap_or_default(),
        },
    })
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
    let switched = prev_displayed != Some(real_idx);

    let mut dirty = vec![Dirty::Scalar(ScalarField::SelectedThread)];

    if switched {
        // Save outgoing draft; restore incoming draft into the active buffer.
        if let Some(prev) = prev_displayed {
            if let Some(thread) = model.threads.get_mut(prev) {
                thread.compose_draft = std::mem::take(&mut model.compose_text);
            }
        } else if !model.compose_text.is_empty() {
            // No prior displayed thread but global compose has text — keep
            // it on the newly selected thread only after switch.
        }
        model.compose_text = model
            .threads
            .get(real_idx)
            .map(|thread| thread.compose_draft.clone())
            .unwrap_or_default();
        dirty.push(Dirty::Scalar(ScalarField::ComposeText));

        // Force next selected_thread_snapshot to treat this as a switch
        // (resync messages/terminals/pending even if target cache is empty).
        model.displayed_thread = None;

        let old_msg_keys = model.message_model_keys.borrow().clone();
        if !old_msg_keys.is_empty() {
            dirty.push(Dirty::MessagesDiff {
                thread_id: String::new(),
                ops: crate::dirty::diff_by_id(
                    &old_msg_keys,
                    &[],
                    &Vec::<crate::MessageItem>::new(),
                ),
            });
        }
        dirty.push(Dirty::PendingRequest {
            thread_id: String::new(),
        });
        dirty.push(Dirty::Error {
            thread_id: String::new(),
            detail: crate::dirty::ErrorDetail {
                message: String::new(),
            },
        });
        dirty.push(Dirty::Terminal {
            id: String::new(),
        });
        dirty.push(Dirty::LocalTerminal);
    }

    let thread_id = model
        .threads
        .get(real_idx)
        .map(|thread| thread.thread_id.clone());
    (
        thread_id
            .map(|thread_id| vec![Effect::PersistSelectedThread { thread_id }])
            .unwrap_or_default(),
        dirty,
    )
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
            // Auto-detected by family rather than an enumerated literal
            // list: `gateway_urls`/`spawn_gateway_process` only ever
            // recognize two provider keys today ("codex"/"claude" --
            // see AgentBridge's constructor, which resolves exactly
            // these two), so any agent id naming the claude family (the
            // short label "claude" this reducer's own gateway wiring
            // uses elsewhere, the real registry id "claude-acp", a
            // hypothetical "claude-code", ...) maps to it; anything else
            // defaults to codex, same as before. Found live via a real
            // settings.global.json with default_agent_id: "claude-acp"
            // (plausibly persisted by a picker backed by the agent
            // catalog's real registry ids, not the short label), which
            // an exact-match list previously silently routed to codex.
            let provider = if model
                .default_agent_id
                .to_ascii_lowercase()
                .contains("claude")
            {
                "claude"
            } else {
                "codex"
            }
            .to_owned();
            // The literal string "default" is a reserved sentinel, never a
            // real profile name -- see settings_file.rs's
            // non_default_sentinel and acpxmgr.go's WriteConfig doc
            // comment (the "snapshotd-mcp-attach" profile's own agent_id
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
            let profile_name = (!model.default_profile.is_empty()
                && model.default_profile != "default")
                .then(|| model.default_profile.clone());
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
        ThreadMsg::ArchiveRequested(idx) => {
            let Some(thread) = model.threads.get_mut(idx) else {
                return (vec![], vec![]);
            };
            thread.archived = true;
            (
                vec![Effect::ArchiveThread { real_index: idx }],
                vec![Dirty::ThreadRow(idx)],
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
                vec![Effect::ToggleBackground { real_index: idx }],
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
            let thread_id =
                thread_id.unwrap_or_else(|| format!("thread:{}", model.threads.len()));
            model.threads.push(ThreadModel {
                thread_id: thread_id.clone(),
                display_name: title,
                provider: provider.clone(),
                session_id: Some(session_id.clone()),
                send_queue: new_thread_send_queue(&thread_id),
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

/// Rebuild transcript + send-queue projection after a queue mutation and
/// emit the matching `MessagesDiff` dirty set.
fn rebuild_send_queue_projection(
    model: &mut Model,
    idx: usize,
) -> (String, Vec<Dirty>) {
    let expanded = model.expanded.clone();
    let Some(thread) = model.threads.get_mut(idx) else {
        return (String::new(), vec![]);
    };
    let thread_id = thread.thread_id.clone();
    let old_keys = thread.transcript_keys.clone();
    let in_flight = matches!(
        thread.state,
        ThreadState::Loading | ThreadState::Cancelling
    );
    let (rows, keys) = crate::models::message_rows_for_thread_with_state(
        thread.transcript.clone(),
        &expanded,
        &thread.send_queue,
        in_flight,
    );
    let ops = crate::dirty::diff_by_id(&old_keys, &keys, &rows);
    thread.message_rows = rows;
    thread.transcript_keys = keys;
    (
        thread_id.clone(),
        vec![
            Dirty::ThreadRow(idx),
            Dirty::MessagesDiff {
                thread_id,
                ops,
            },
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
                    Ok(_) => {
                        // Rebuild message projection with queue rows so
                        // QueuedMessageBar appears immediately.
                        let expanded = model.expanded.clone();
                        let old_keys = thread.transcript_keys.clone();
                        let in_flight = matches!(
                            thread.state,
                            ThreadState::Loading | ThreadState::Cancelling
                        );
                        let (rows, keys) = crate::models::message_rows_for_thread_with_state(
                            thread.transcript.clone(),
                            &expanded,
                            &thread.send_queue,
                            in_flight,
                        );
                        let ops = crate::dirty::diff_by_id(&old_keys, &keys, &rows);
                        thread.message_rows = rows;
                        thread.transcript_keys = keys;
                        (
                            vec![],
                            vec![
                                Dirty::ThreadRow(idx),
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
            // Sending resumes auto-processing after a manual stop.
            thread.send_queue.resume();
            (
                vec![Effect::SendPrompt {
                    real_index: idx,
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
                    Dirty::ThreadRow(idx),
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
                let is_generating = matches!(
                    thread.state,
                    ThreadState::Loading | ThreadState::Cancelling
                );
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
                    dirty.push(Dirty::Connection { thread_id });
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
                        real_index: idx,
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
        SettingsMsg::ProfileSelected(profile_name) => {
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
            // it to -- open_session_maybe_profiled reads this straight
            // from the model once the thread actually opens.
            if thread.session_id.is_some() {
                return (vec![], vec![]);
            }
            thread.profile_name = Some(profile_name);
            (vec![], vec![Dirty::ThreadRow(idx), Dirty::Capabilities {
                thread_id: thread.thread_id.clone(),
            }])
        }
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
        SettingsMsg::McpServerAuthenticate { name } => (
            vec![Effect::McpServerAuthenticate {
                real_index: idx,
                name,
            }],
            vec![Dirty::Settings],
        ),
        SettingsMsg::McpServerToolEnabledChanged {
            server_name,
            tool_name,
            enabled,
        } => (
            vec![Effect::McpServerToolEnabledChanged {
                real_index: idx,
                server_name,
                tool_name,
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
        SettingsMsg::AgentSetEnabled { agent_id, enabled } => (
            vec![Effect::AgentSetEnabled {
                real_index: idx,
                agent_id,
                enabled,
            }],
            vec![Dirty::Settings],
        ),
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
            (
                vec![Effect::SkillWrite { path, content }],
                vec![Dirty::SkillEditor],
            )
        }
        SkillMsg::CopyPathRequested { path } => (
            vec![Effect::ClipboardWrite {
                text: path.to_string_lossy().into_owned(),
            }],
            vec![],
        ),
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
            let terminals_model = model.terminals_model.clone();
            let thread_keys = model.thread_model_keys.borrow().clone();
            let message_keys = model.message_model_keys.borrow().clone();
            let skill_keys = model.skill_model_keys.borrow().clone();
            let profile_keys = model.profile_model_keys.borrow().clone();
            let mcp_server_keys = model.mcp_server_model_keys.borrow().clone();
            let agent_catalog_keys = model.agent_catalog_model_keys.borrow().clone();
            let recoverable_session_keys = model.recoverable_session_model_keys.borrow().clone();
            let terminal_keys = model.terminal_model_keys.borrow().clone();
            let startup_warnings = initial.startup_warnings.clone();
            *model = Model::from_initial_state(initial);
            model.thread_model = thread_model;
            model.messages_model = messages_model;
            model.skills_model = skills_model;
            model.profiles_model = profiles_model;
            model.mcp_servers_model = mcp_servers_model;
            model.agent_catalog_model = agent_catalog_model;
            model.recoverable_sessions_model = recoverable_sessions_model;
            model.terminals_model = terminals_model;
            *model.thread_model_keys.borrow_mut() = thread_keys.clone();
            *model.message_model_keys.borrow_mut() = message_keys;
            *model.skill_model_keys.borrow_mut() = skill_keys;
            *model.profile_model_keys.borrow_mut() = profile_keys;
            *model.mcp_server_model_keys.borrow_mut() = mcp_server_keys;
            *model.agent_catalog_model_keys.borrow_mut() = agent_catalog_keys;
            *model.recoverable_session_model_keys.borrow_mut() = recoverable_session_keys;
            *model.terminal_model_keys.borrow_mut() = terminal_keys;
            let thread_list_dirty = thread_list_dirty_with_keys(model, thread_keys);
            // Cold start: everything is dirty, there is no prior row
            // identity to preserve (see 00-plan.md's known-gap section).
            let mut dirty = vec![
                thread_list_dirty,
                Dirty::Scalar(ScalarField::SelectedThread),
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
        // Skills list is refreshed by effect_executor before this
        // result is folded (see CreateSkill's refresh-before-open
        // order); do not emit an empty SkillsListDiff here -- that
        // would re-push the pre-create list and race the real rescan.
        EffectResultMsg::SkillCreated(Ok(path)) => (vec![Effect::OpenSkillEditor { path }], vec![]),
        EffectResultMsg::SkillWritten(Ok(())) => {
            model.skill_saving = false;
            (vec![], vec![Dirty::SkillEditor])
        }
        EffectResultMsg::SkillPromoted(Ok(())) => (vec![], vec![]),
        EffectResultMsg::ExternalEditorOpened(Ok(()))
        | EffectResultMsg::OsDefaultOpened(Ok(())) => (vec![], vec![]),
        EffectResultMsg::SkillEditorLoaded(Ok(state)) => {
            model.active_skill_name = state.name;
            model.active_skill_path = state.path;
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
        EffectResultMsg::SkillWritten(Err(err)) => {
            model.skill_saving = false;
            (
                vec![],
                vec![
                    Dirty::SkillEditor,
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
        | EffectResultMsg::OsDefaultOpened(Err(err)) => (
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
    for (event_index, bridge_event) in frame.bridge_events.iter().enumerate() {
        let target_index = frame
            .bridge_event_thread_ids
            .get(event_index)
            .filter(|thread_id| !thread_id.is_empty())
            .and_then(|thread_id| {
                model
                    .threads
                    .iter()
                    .position(|thread| Model::thread_matches_id(thread, thread_id))
            })
            .unwrap_or(bridge_event.thread_index);
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
                if matches!(
                    message.kind,
                    crate::protocol_types::MessageKind::Agent
                        | crate::protocol_types::MessageKind::ToolCall
                ) {
                    thread.agent_content_this_turn = true;
                }
                dirty.push(Dirty::MessageAppended {
                    thread_id: thread.thread_id.clone(),
                });
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
                        real_index: target_index,
                        text: entry.text,
                    });
                }
                dirty.push(Dirty::ThreadRow(target_index));
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
                dirty.push(Dirty::ThreadRow(target_index));
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
                ops: crate::dirty::diff_by_id(&old_keys, &[], &Vec::<crate::MessageItem>::new()),
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
            crate::models::to_message_rows_from_transcript(snapshot.transcript.clone(), &[]).len();
        if model.expanded.len() < transcript_row_count {
            model.expanded.resize(transcript_row_count, false);
        }
        let expanded = model.expanded.clone();
        if let Some(thread) = model.threads.get_mut(target_index) {
            let thread_id = thread.thread_id.clone();
            let old_keys = thread.transcript_keys.clone();
            // Include send-queue rows (QueuedMessageBar) in the projection.
            let in_flight = matches!(
                thread.state,
                ThreadState::Loading | ThreadState::Cancelling
            );
            let (rows, new_keys) = crate::models::message_rows_for_thread_with_state(
                snapshot.transcript.clone(),
                &expanded,
                &thread.send_queue,
                in_flight,
            );
            // `old_keys`/`thread.message_rows` are this thread's *own*
            // previously-cached copy, not what's actually still on screen.
            // A brand new thread's own cache is empty both before and
            // after this snapshot, so without `switched_thread` here the
            // diff below never fires on switch -- the shared
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
            // dispatches its own `Dirty::MessagesDiff` explicitly (see
            // `ChromeMsg::ToggleExpanded`).
            let transcript_changed =
                switched_thread || old_keys != new_keys || thread.transcript != snapshot.transcript;
            // Same "own cache vs. what's actually still on screen" gap as
            // `transcript_changed` above applies to every other
            // per-thread view fragment: force a resync on switch even when
            // the target thread's own diff is a no-op.
            let pending_changed =
                switched_thread || thread.pending_request != snapshot.pending_request;
            let terminals_changed = switched_thread
                || thread.terminals != snapshot.terminals
                || thread.expanded_terminal != snapshot.expanded_terminal;
            let local_terminal_changed =
                switched_thread || thread.local_terminal != snapshot.local_terminal;
            let local_terminal_output_changed =
                thread.local_terminal.screen_text != snapshot.local_terminal.screen_text;
            let connection_changed =
                switched_thread || thread.connection_status != snapshot.connection_status;
            let capabilities_changed = switched_thread
                || thread.session_modes != snapshot.session_modes
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
    // row_count()/row_data() on the persistent messages_model VecModel.
    use slint::Model as _;

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

    /// Dirty set emitted when selection actually changes thread (leak-fix
    /// clear of compose/pending/error/terminals for the outgoing row).
    fn thread_switch_dirty() -> Vec<Dirty> {
        vec![
            Dirty::Scalar(ScalarField::SelectedThread),
            Dirty::Scalar(ScalarField::ComposeText),
            Dirty::PendingRequest {
                thread_id: String::new(),
            },
            Dirty::Error {
                thread_id: String::new(),
                detail: ErrorDetail {
                    message: String::new(),
                },
            },
            Dirty::Terminal {
                id: String::new(),
            },
            Dirty::LocalTerminal,
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
        assert_eq!(dirty, thread_switch_dirty());
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
        assert_eq!(dirty, thread_switch_dirty());
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
        assert_eq!(dirty, thread_switch_dirty());
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
        assert_eq!(dirty, thread_switch_dirty());
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
        assert_eq!(dirty, vec![Dirty::ThreadRow(0)]);
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
        // Selection change also dirties compose/pending/error/terminals so
        // the outgoing thread's UI state cannot leak (apply_thread_selection_switch).
        assert_eq!(dirty, thread_switch_dirty());
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

    /// setup-followups plan, provider_fastmode_profile_persistence: the
    /// compose-bar profile picker is only ever meant to be interactive
    /// (per ThreadItem.has-session) while the selected thread has no
    /// attached session yet -- this proves the reducer itself enforces
    /// that, not just the Slint-side `enabled:` gate (a UI-only lock
    /// would still let a stale/racing dispatch mutate the model).
    #[test]
    fn profile_selected_updates_the_thread_only_while_it_has_no_session() {
        let mut model = model_with_threads(&["a"]);
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Settings(SettingsMsg::ProfileSelected(
                "codex-tools".to_owned(),
            ))),
        );
        assert_eq!(model.threads[0].profile_name.as_deref(), Some("codex-tools"));
        assert!(effects.is_empty(), "no backend to notify yet -- nothing to send");
        assert_eq!(
            dirty,
            vec![
                Dirty::ThreadRow(0),
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
            Msg::Ui(UiMsg::Settings(SettingsMsg::ProfileSelected(
                "balanced".to_owned(),
            ))),
        );
        assert_eq!(
            model.threads[0].profile_name.as_deref(),
            Some("codex-tools"),
            "profile must stay locked once a session has attached"
        );
        assert!(effects.is_empty());
        assert!(dirty.is_empty());
    }

    #[test]
    fn new_thread_provider_matching_auto_detects_any_claude_family_agent_id() {
        // Regression test: a real settings.global.json found live this
        // session had default_agent_id: "claude-acp" (the real registry
        // agent id, plausibly from a picker backed by the agent catalog)
        // rather than the short "claude" label this reducer's own
        // gateway wiring uses everywhere else -- an exact-match list only
        // recognizing "claude"/"claude-code" silently routed everything
        // else, including "claude-acp", to codex. Substring-matching the
        // claude family instead of enumerating every literal variant
        // means a *hypothetical future* id ("Claude-Opus-Next", picked
        // case-insensitively) is covered automatically too, without this
        // match needing to grow a new arm every time one shows up.
        for agent_id in [
            "claude",
            "claude-code",
            "claude-acp",
            "Claude-Opus-Next", // not a real id -- proves this generalizes, not just today's known set
        ] {
            let mut model = model_with_threads(&[]);
            model.default_agent_id = agent_id.to_owned();
            let (effects, _) = update(&mut model, Msg::Ui(UiMsg::Thread(ThreadMsg::New)));
            assert!(
                matches!(
                    effects.as_slice(),
                    [Effect::NewThread { provider, .. }] if provider == "claude"
                ),
                "default_agent_id {agent_id:?} must route new threads to the claude provider, \
                 got: {effects:?}"
            );
        }
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
    fn new_thread_never_forwards_the_literal_default_profile_sentinel() {
        // Regression test: "agent default is in crash backoff". The
        // literal string "default" is a reserved acpx-server sentinel
        // (see acpxmgr.go's WriteConfig doc comment: the
        // "snapshotd-mcp-attach" profile's own agent_id is deliberately
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
            vec![Effect::NewThread {
                real_index: 1,
                display_name: "New thread 2".to_owned(),
                provider: "codex".to_owned(),
                profile_name: None,
                permission_profile: None,
            }],
            "a literal \"default\" profile/permission-profile must never reach session/new"
        );
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
                real_index: 0,
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
            dirty.contains(&Dirty::ThreadRow(0)),
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
                ..FrameInput::default()
            }),
        );
        // State stays Idle (user can just re-send), but the empty turn
        // is called out via the error surface.
        assert_eq!(model.threads[0].state, ThreadState::Idle);
        let error = model.threads[0].error.as_deref().expect("empty-turn notice set");
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
        let (rows, keys) = crate::models::message_rows_for_thread_with_state(
            model.threads[0].transcript.clone(),
            &expanded,
            &model.threads[0].send_queue,
            true,
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
            dirty.iter().any(|d| matches!(d, Dirty::MessagesDiff { .. })),
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
        let (rows, keys) = crate::models::message_rows_for_thread_with_state(
            model.threads[0].transcript.clone(),
            &expanded,
            &model.threads[0].send_queue,
            true,
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
        let (rows, keys) = crate::models::message_rows_for_thread_with_state(
            model.threads[0].transcript.clone(),
            &expanded,
            &model.threads[0].send_queue,
            false,
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
                real_index: 0,
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
        let (rows, keys) = crate::models::message_rows_for_thread_with_state(
            model.threads[0].transcript.clone(),
            &expanded,
            &model.threads[0].send_queue,
            true,
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
                    real_index: 0,
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
        assert!(popped.is_none(), "AbsorbingCancel must swallow this Stopped event");
        assert_eq!(model.threads[0].send_queue.len(), 1);
    }

    #[test]
    fn queue_stop_pauses_queue_and_cancels_generation() {
        let mut model = model_with_threads(&["a"]);
        model.threads[0].state = ThreadState::Loading;
        model.threads[0]
            .send_queue
            .enqueue("waiting".to_owned(), false)
            .expect("queue");
        let (effects, dirty) = update(
            &mut model,
            Msg::Ui(UiMsg::Compose(ComposeMsg::QueueStop)),
        );
        assert_eq!(
            effects,
            vec![Effect::CancelGeneration { real_index: 0 }]
        );
        assert_eq!(model.threads[0].state, ThreadState::Cancelling);
        assert!(dirty.contains(&Dirty::ThreadRow(0)));
        // Paused: TurnEnded must not auto-drain.
        let (effects2, _) = update(
            &mut model,
            Msg::Frame(FrameInput {
                bridge_events: vec![crate::agent_bridge::BridgeEvent {
                    thread_index: 0,
                    event: crate::protocol_types::AgentEvent::TurnEnded("cancelled".to_owned()),
                }],
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
                real_index: 1,
                text: "queued".to_owned(),
            }]
        );
        assert_eq!(model.threads[0].thread_id, "other-id");
        assert_eq!(model.threads[1].thread_id, "target-id");
        assert_eq!(model.threads[1].state, ThreadState::Loading);
        assert!(dirty.contains(&Dirty::ThreadRow(1)));
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
                    startup_warnings: vec![],
                    send_queues: vec![],
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
            local_terminal: crate::LocalTerminalItem::default(),
            connection_status: String::new(),
            session_modes: None,
            config_options: vec![],
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
    fn switching_to_a_thread_with_a_coincidentally_unchanged_transcript_still_resyncs_the_shared_model() {
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
                    local_terminal: crate::LocalTerminalItem::default(),
                    connection_status: String::new(),
                    session_modes: None,
                    config_options: vec![],
                }),
                ..FrameInput::default()
            }),
        );

        assert_eq!(model.displayed_thread, Some(1));
        let ops = dirty
            .iter()
            .find_map(|item| match item {
                Dirty::MessagesDiff { thread_id, ops } if thread_id == "thread-1" => {
                    Some(ops.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| {
                panic!(
                    "switching to the new thread must resync the shared messages model even \
                     though thread-1's own transcript diff is a no-op -- otherwise thread-0's \
                     messages stay on screen as bogus 'prefill' data: {dirty:?}"
                )
            });

        // The marker alone doesn't prove anything ends up on screen --
        // actually apply it, the same way sync() would, and check the
        // *shared* model, not per-thread state.
        crate::sync::apply_message_ops(&model, "thread-1", &ops);
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
        // result is folded; SkillCreated itself only opens the editor.
        assert!(dirty.is_empty());
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
