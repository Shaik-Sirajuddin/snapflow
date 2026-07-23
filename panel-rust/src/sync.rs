//! `tea-slint-model` Phase 3: `sync(&Model, &ChatPanel, &[Dirty])` -- the
//! **sole** owner of pushing `Model` state into Slint `set_*` setters.
//! See `memory/rui/gen/plans/tea-slint-model/00-plan.md`.
//!
//! Dispatchers, effect completions, and cold-start/frame hydration call
//! `sync()` for their returned Dirty markers. Bridge-backed messages,
//! terminals, requests, capabilities, settings lists, skills, and host
//! appearance are first folded into `Model` snapshots before projection.
//!
//! The match below is exhaustive with **no wildcard arm**, matching
//! `update()`'s own requirement (00-plan.md's "Exhaustiveness
//! requirement") -- a new `Dirty` variant without a handling arm here
//! must fail to compile.

use crate::dirty::{Dirty, RowOp};
use crate::model::Model;
use crate::ChatPanel;
use slint::{Global, Model as SlintModel};

pub fn sync(model: &Model, component: &ChatPanel, dirty: &[Dirty]) {
    for d in dirty {
        sync_one(model, component, d);
    }
}

fn trace_transcript_tail(model: &Model, thread_id: &str) {
    if std::env::var_os("RUI_PANEL_INPUT_TRACE").is_none() {
        return;
    }
    let Some(idx) = displayed_thread_for_id(model, thread_id) else {
        return;
    };
    let Some(thread) = model.threads.get(idx) else {
        return;
    };
    let items = thread.transcript.iter().rev().take(3).collect::<Vec<_>>();
    for item in items.into_iter().rev() {
        let (kind, text) = match item {
            crate::conversation::TranscriptItem::User { text, .. } => ("user", text),
            crate::conversation::TranscriptItem::Assistant { text, .. } => ("agent", text),
            crate::conversation::TranscriptItem::Thought { text, .. } => ("thinking", text),
            crate::conversation::TranscriptItem::Tool { title, .. } => ("tool_use", title),
            crate::conversation::TranscriptItem::Terminal { output, .. } => ("terminal", output),
            crate::conversation::TranscriptItem::Notice { text } => ("notice", text),
        };
        let preview: String = text.chars().take(60).collect();
        crate::trace_host_input(format_args!(
            "transcript thread={} kind={} text={:?}",
            idx, kind, preview
        ));
    }
}

fn sync_one(model: &Model, component: &ChatPanel, dirty: &Dirty) {
    match dirty {
        Dirty::Scalar(field) => sync_scalar(model, component, *field),
        Dirty::ThreadRow(idx) => {
            apply_thread_row(model, *idx);
        }
        Dirty::ThreadListDiff(ops) => {
            apply_thread_ops(model, ops);
            // Phase 19: section counters (active vs archived) follow every
            // thread-list rebuild.
            let archived = model.threads.iter().filter(|t| t.archived && !t.closed).count() as i32;
            let active = model.threads.iter().filter(|t| !t.archived && !t.closed).count() as i32;
            component.set_active_thread_count(active);
            component.set_archived_thread_count(archived);
        }
        Dirty::MessageAppended { thread_id } => {
            sync_message_snapshot(model, thread_id);
            sync_has_older_messages(model, component, thread_id);
            trace_transcript_tail(model, thread_id);
        }
        Dirty::MessagesDiff { thread_id, ops } => {
            // `apply_message_ops` treats an empty `thread_id` as "no
            // thread is selected" and force-converges the shared
            // `messages_model` down to zero rows -- correct for
            // `update_frame`'s `frame.clear_selected_thread` path (which
            // always pairs an empty `thread_id` with real `Remove` ops
            // from a genuine old-keys-to-empty diff). But
            // `update_frame`'s `frame.bridge_events_pending` block pushes
            // this exact same empty-`thread_id` sentinel with *empty*
            // ops too, as a generic "some thread (don't know which) had
            // background activity" no-op signal -- not "no thread is
            // selected". Regression: a real thread IS displayed and has
            // real messages on screen; routing that no-op signal through
            // `apply_message_ops` anyway silently wiped every visible
            // message the instant *any* thread's bridge activity
            // (including an unrelated background thread) coincided with
            // the selected thread's own content having already settled
            // (no compensating non-empty diff that same tick to mask
            // it) -- reproduced headlessly as "the sent message
            // disappeared a few poll ticks after being sent". An empty
            // `thread_id` with empty `ops` carries no actual instruction
            // to apply; skip it entirely rather than let it fall through.
            if !thread_id.is_empty() || !ops.is_empty() {
                apply_message_ops(model, thread_id, ops);
            }
            sync_has_older_messages(model, component, thread_id);
            if !thread_id.is_empty() {
                trace_transcript_tail(model, thread_id);
            }
        }
        Dirty::MessageStreamingDelta {
            thread_id,
            message_id,
            delta,
        } => {
            apply_message_streaming(model, thread_id, message_id, delta);
            sync_has_older_messages(model, component, thread_id);
        }
        Dirty::Connection { thread_id } => {
            if let Some(thread) = thread_for_id(model, thread_id) {
                component.set_connection_status(thread.connection_status.clone().into());
            }
        }
        Dirty::Toast => {
            component.set_toast_message(model.toast_message.clone().into());
            component.set_toast_kind(model.toast_kind.clone().into());
            component.set_toast_seq(model.toast_seq);
        }
        Dirty::Error { thread_id, detail } => {
            // Match durable thread_id *or* session_id (same contract as
            // MessageStreamingDelta / frame snapshots). Comparing only
            // session_id dropped banners for pre-attach threads.
            let for_displayed = thread_id.is_empty()
                || model.displayed_thread.is_some_and(|idx| {
                    model
                        .threads
                        .get(idx)
                        .is_some_and(|thread| Model::thread_matches_id(thread, thread_id))
                });
            if for_displayed {
                component.set_last_error(detail.message.clone().into());
            }
        }
        Dirty::PendingRequest { thread_id } => {
            // Empty id = transition clear on thread switch (leak_audit §2.3).
            if thread_id.is_empty() {
                component.set_pending_request(crate::PendingRequestItem::default());
            } else if let Some(thread) = thread_for_id(model, thread_id) {
                component.set_pending_request(thread.pending_request.clone());
            }
        }
        Dirty::Terminal { .. } => {
            if let Some(idx) = model.displayed_thread {
                if let Some(thread) = model.threads.get(idx) {
                    reconcile_terminals(model, &thread.terminals);
                    component.set_terminals(slint::ModelRc::from(model.terminals_model.clone()));
                    if let Some(expanded) = &thread.expanded_terminal {
                        component.set_expanded_terminal(expanded.clone());
                    } else {
                        component.set_expanded_terminal(crate::TerminalItem::default());
                    }
                }
            } else {
                // Transition clear before the new thread's snapshot lands.
                reconcile_terminals(model, &[]);
                component.set_terminals(slint::ModelRc::from(model.terminals_model.clone()));
                component.set_expanded_terminal(crate::TerminalItem::default());
            }
        }
        Dirty::LocalTerminal => {
            if let Some(idx) = model.displayed_thread {
                if let Some(thread) = model.threads.get(idx) {
                    component.set_local_terminal(thread.local_terminal.clone());
                }
            } else {
                component.set_local_terminal(crate::LocalTerminalItem::default());
            }
        }
        Dirty::ProjectPath => {
            component.set_active_project_path(
                model.active_project_path.clone().unwrap_or_default().into(),
            );
        }
        Dirty::Appearance => sync_appearance(model, component),
        Dirty::Theme => sync_theme_variant(component, &model.theme_variant),
        Dirty::Settings => {
            component.set_settings_scope(model.settings_scope.clone().into());
            component.set_default_profile(model.default_profile.clone().into());
            component.set_permission_profile(model.permission_profile.clone().into());
            component.set_background_default(model.background_default);
            component.set_default_agent_id(model.default_agent_id.clone().into());
            component.set_dev_mode(model.dev_mode);
            component.set_background_override_set(model.background_override_set);
            component.set_background_override(model.background_override);
            reconcile_settings_models(model, component);
            // model.available_profiles (the compose-bar profile picker's
            // own data source) is refreshed here, via the periodic
            // settings-gateway snapshot poll -- not tied to the
            // currently-selected thread's own Dirty::Capabilities at
            // all. Without this, a picker whose profile list was still
            // empty at startup would never pick up the real list once it
            // arrived until some unrelated capability change or thread
            // switch happened to also fire.
            let real_idx = crate::update::selected_real_index(model);
            if let Some(thread) = model.threads.get(real_idx) {
                sync_profile_picker(model, component, thread);
            }
        }
        Dirty::SkillsListDiff(ops) => {
            apply_skill_ops(model, ops);
            component.set_available_skills(slint::ModelRc::from(model.skills_model.clone()));
        }
        Dirty::SkillRow(idx) => {
            if *idx < model.skills_model.row_count() {
                if let Some(row) = model.skills_model.row_data(*idx) {
                    model.skills_model.set_row_data(*idx, row);
                }
            }
        }
        Dirty::SkillEditor => sync_skill_editor_state(model, component),
        Dirty::Capabilities { thread_id } => {
            if let Some(thread) = thread_for_id(model, thread_id) {
                component.set_mode_trigger_label(
                    crate::models::current_mode_name(&thread.session_modes).into(),
                );
                component.set_mode_dropdown_entries(crate::models::to_mode_dropdown_entries(
                    thread.session_modes.clone(),
                ));
                component.set_reasoning_trigger_label(
                    crate::models::current_reasoning_trigger_label(&thread.config_options).into(),
                );
                component.set_reasoning_dropdown_entries(
                    crate::models::to_reasoning_dropdown_entries(thread.config_options.clone()),
                );
                let fast = crate::models::fast_mode_from_config(&thread.config_options);
                component.set_fast_mode_available(fast.available);
                component.set_fast_mode_enabled(fast.enabled);
                component.set_fast_mode_option_id(fast.option_id.into());
                component.set_fast_mode_on_value(fast.on_value.into());
                component.set_fast_mode_off_value(fast.off_value.into());
                sync_profile_picker(model, component, thread);
                // Model list after provider resolve so it can filter by agent.
                sync_model_dropdown_for_provider(model, component, thread);
                // Phase 18: live token usage -> compose context ring,
                // streaming DURING a turn (usage folds through the
                // frame snapshot; Capabilities dirty fires on change).
                component.set_context_used_tokens(thread.usage.0 as i32);
                component.set_context_limit_tokens(thread.usage.1 as i32);
                component.set_context_ratio(if thread.usage.1 > 0 {
                    (thread.usage.0 as f32 / thread.usage.1 as f32).clamp(0.0, 1.0)
                } else { 0.0 });
            }
        }
    }
}

fn sync_skill_editor_state(model: &Model, component: &ChatPanel) {
    component.set_active_skill_name(model.active_skill_name.clone().into());
    component.set_active_skill_path(model.active_skill_path.clone().into());
    component.set_active_skill_content(model.active_skill_content.clone().into());
    // Phase 27: markdown preview of the active content for the editor's
    // Preview toggle.
    component.set_active_skill_markdown(crate::models::skill_markdown_preview(
        &model.active_skill_content,
    ));
    component.set_skill_saving(model.skill_saving);
    let editors = model
        .detected_editors
        .iter()
        .cloned()
        .map(Into::into)
        .collect::<Vec<slint::SharedString>>();
    component.set_detected_editors(slint::ModelRc::new(slint::VecModel::from(editors)));
    component.set_active_pane(model.active_pane.clone().into());
}

// setup-followups plan, archive_thread_backend_verify: pub(crate), same
// reasoning as apply_message_ops -- lets agent_bridge.rs's real-backend
// test drive this exact reducer/sync path directly.
pub(crate) fn apply_thread_row(model: &Model, real_index: usize) {
    let mut keys = model.thread_model_keys.borrow_mut();
    let Some(thread) = model.threads.get(real_index) else {
        return;
    };
    // Resolve the sidebar slot by durable id first, then by the synthetic
    // cold-start key, then by visible_indices / thread_rows real_index.
    // A pure thread_id lookup used to no-op when bridge-assigned durable
    // ids and model.threads.thread_id were briefly out of sync — the
    // exact "thread list does not update on send" failure mode.
    let durable = thread.thread_id.as_str();
    let synthetic = format!("thread:{real_index}");
    let row_index = keys
        .iter()
        .position(|key| key == durable || key == synthetic.as_str())
        .or_else(|| {
            model
                .visible_indices
                .iter()
                .position(|idx| *idx == real_index)
        })
        .or_else(|| {
            model
                .thread_rows
                .iter()
                .position(|row| row.real_index == real_index)
        });
    let Some(row_index) = row_index else {
        return;
    };
    // Recompute from live model.threads (status/busy), preserving
    // snapshot-filled display fields inside visible_thread_row.
    let Some(row) = crate::update::visible_thread_row(model, real_index) else {
        return;
    };
    let needs_write = model
        .thread_model
        .row_data(row_index)
        .map(|existing| {
            existing.name != row.item.name
                || existing.status != row.item.status
                || existing.busy != row.item.busy
                || existing.open != row.item.open
                || existing.background != row.item.background
                || existing.description != row.item.description
                || existing.closed != row.item.closed
                || existing.archived != row.item.archived
                || existing.provider != row.item.provider
                || existing.model != row.item.model
                || existing.project_path != row.item.project_path
                || existing.project_name != row.item.project_name
                || existing.profile_name != row.item.profile_name
                || existing.has_session != row.item.has_session
        })
        .unwrap_or(true);
    if needs_write {
        model
            .thread_model
            .set_row_data(row_index, row.item.clone());
    }
    // Keep the key cache on the durable id so the next lookup and
    // ThreadListDiff reconcile don't miss this slot.
    if row_index < keys.len() {
        keys[row_index] = row.thread_id.clone();
    }
}

fn thread_for_id<'a>(model: &'a Model, thread_id: &str) -> Option<&'a crate::model::ThreadModel> {
    model
        .threads
        .iter()
        .find(|thread| Model::thread_matches_id(thread, thread_id))
}

fn displayed_thread_for_id(model: &Model, thread_id: &str) -> Option<usize> {
    model.displayed_thread.filter(|idx| {
        model
            .threads
            .get(*idx)
            .is_some_and(|thread| Model::thread_matches_id(thread, thread_id))
    })
}

fn sync_message_snapshot(model: &Model, thread_id: &str) {
    let Some(idx) = displayed_thread_for_id(model, thread_id) else {
        return;
    };
    let Some(thread) = model.threads.get(idx) else {
        return;
    };
    let old_keys = model.message_model_keys.borrow().clone();
    let ops = crate::dirty::diff_by_id(&old_keys, &thread.transcript_keys, &thread.message_rows);
    apply_message_ops(model, thread_id, &ops);
}

fn sync_has_older_messages(model: &Model, component: &ChatPanel, thread_id: &str) {
    if let Some(idx) = displayed_thread_for_id(model, thread_id) {
        if let Some(thread) = model.threads.get(idx) {
            component.set_has_older_messages(thread.has_older_messages);
        }
    }
}

fn apply_message_streaming(model: &Model, thread_id: &str, message_id: &str, delta: &str) {
    if displayed_thread_for_id(model, thread_id).is_none() {
        return;
    }
    let candidates = [
        format!("assistant:{message_id}"),
        format!("thought:{message_id}"),
        format!("user:{message_id}"),
        format!("tool:{message_id}"),
    ];
    let keys = model.message_model_keys.borrow();
    let Some(index) = keys
        .iter()
        .position(|key| candidates.iter().any(|candidate| candidate == key))
    else {
        return;
    };
    let Some(mut row) = model.messages_model.row_data(index) else {
        return;
    };
    row.text = format!("{}{}", row.text, delta).into();
    model.messages_model.set_row_data(index, row);
}

fn apply_thread_ops(model: &Model, ops: &[RowOp<crate::models::VisibleThreadItem>]) {
    let mut keys = model.thread_model_keys.borrow_mut();
    for op in ops {
        match op {
            RowOp::Insert { at, row } => {
                // `diff_by_id` computes `at` against the key-cache length at
                // the time the ops were built. If something else touched
                // `thread_model`/`keys` between then and now (a bug this
                // clamp doesn't fix, but shouldn't crash the whole app over
                // either -- this crate is `panic = "abort"`, so an
                // out-of-bounds insert here used to kill the process on any
                // message send), clamp to the current length instead of
                // panicking.
                let at = (*at).min(model.thread_model.row_count()).min(keys.len());
                model.thread_model.insert(at, row.item.clone());
                keys.insert(at, row.thread_id.clone());
            }
            RowOp::Remove { at } => {
                if *at < model.thread_model.row_count() && *at < keys.len() {
                    model.thread_model.remove(*at);
                    keys.remove(*at);
                }
            }
            RowOp::Move { from, to } => {
                if *from < model.thread_model.row_count()
                    && *to <= model.thread_model.row_count()
                    && *from < keys.len()
                    && *to <= keys.len()
                {
                    let row = model.thread_model.remove(*from);
                    model.thread_model.insert(*to, row);
                    let key = keys.remove(*from);
                    keys.insert(*to, key);
                }
            }
        }
    }
    // Ops are computed from a snapshot of `keys` taken earlier in the same
    // turn. If anything mutated `thread_model`/`keys` out from under that
    // snapshot -- a bug elsewhere, or two dirty events racing on the same
    // model -- applying stale ops above can leave the row count short of,
    // or past, the real desired length. Previously that showed up as an
    // `assert_eq!` abort (this crate is `panic = "abort"`); the clamps
    // above stop the abort but, left alone, a persistent desync would just
    // silently keep inserting rows every poll tick forever (duplicate
    // sidebar entries that never go away). Force convergence to
    // `model.thread_rows` -- the actual source of truth -- so any drift
    // self-heals within one frame instead of accumulating.
    let desired_len = model.thread_rows.len();
    while model.thread_model.row_count() > desired_len {
        model.thread_model.remove(model.thread_model.row_count() - 1);
    }
    while keys.len() > desired_len {
        keys.pop();
    }
    for row in model.thread_rows.iter().skip(model.thread_model.row_count()) {
        model.thread_model.push(row.item.clone());
    }
    for row in model.thread_rows.iter().skip(keys.len()) {
        keys.push(row.thread_id.clone());
    }
    // Length now matches, but existing slots can still hold the wrong
    // *content* (e.g. a stale key left over from before the desync).
    // Only call set_row_data when the row actually changed: Slint's
    // VecModel notifies every set_row_data regardless of value equality,
    // which tears down and recreates `if`-conditional children inside
    // the row (sidebar close/delete arm IconButtons, project badges,
    // etc.). That churn made live MCP element handles go stale on every
    // poll that re-applied an empty/no-op ThreadListDiff -- the exact
    // failure mode setup-followups' thread_row_time_controls e2e hit.
    for (index, row) in model.thread_rows.iter().enumerate() {
        let needs_write = model
            .thread_model
            .row_data(index)
            .map(|existing| {
                existing.name != row.item.name
                    || existing.status != row.item.status
                    || existing.busy != row.item.busy
                    || existing.open != row.item.open
                    || existing.background != row.item.background
                    || existing.description != row.item.description
                    || existing.closed != row.item.closed
                    || existing.archived != row.item.archived
                    || existing.provider != row.item.provider
                    || existing.model != row.item.model
                    || existing.project_path != row.item.project_path
                    || existing.project_name != row.item.project_name
                    || existing.profile_name != row.item.profile_name
                    || existing.has_session != row.item.has_session
            })
            .unwrap_or(true);
        if needs_write {
            model.thread_model.set_row_data(index, row.item.clone());
        }
        keys[index] = row.thread_id.clone();
    }
}

pub(crate) fn apply_message_ops(model: &Model, thread_id: &str, ops: &[RowOp<crate::MessageItem>]) {
    let (desired_keys, desired_rows) = if thread_id.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let Some(idx) = displayed_thread_for_id(model, thread_id) else {
            return;
        };
        (
            model
                .threads
                .get(idx)
                .map(|thread| thread.transcript_keys.clone())
                .unwrap_or_default(),
            model
                .threads
                .get(idx)
                .map(|thread| thread.message_rows.clone())
                .unwrap_or_default(),
        )
    };
    let mut keys = model.message_model_keys.borrow_mut();
    for op in ops {
        match op {
            RowOp::Insert { at, row } => {
                // See apply_thread_ops's comment: clamp instead of trusting
                // `at` against a key cache that may have drifted since the
                // ops were computed -- an out-of-bounds insert here is what
                // was aborting the whole process on message send.
                let at = (*at).min(model.messages_model.row_count()).min(keys.len());
                model.messages_model.insert(at, row.clone());
                keys.insert(at, desired_keys.get(at).cloned().unwrap_or_default());
            }
            RowOp::Remove { at } => {
                if *at < model.messages_model.row_count() && *at < keys.len() {
                    model.messages_model.remove(*at);
                    keys.remove(*at);
                }
            }
            RowOp::Move { from, to } => {
                if *from < model.messages_model.row_count()
                    && *to <= model.messages_model.row_count()
                    && *from < keys.len()
                    && *to <= keys.len()
                {
                    let row = model.messages_model.remove(*from);
                    model.messages_model.insert(*to, row);
                    let key = keys.remove(*from);
                    keys.insert(*to, key);
                }
            }
        }
    }
    // See apply_thread_ops's comment: force convergence to `desired_rows`/
    // `desired_keys` (the actual per-thread transcript) instead of trusting
    // that the ops above landed the model at the right length.
    while model.messages_model.row_count() > desired_rows.len() {
        model
            .messages_model
            .remove(model.messages_model.row_count() - 1);
    }
    while keys.len() > desired_keys.len() {
        keys.pop();
    }
    for key in desired_keys.iter().skip(keys.len()) {
        keys.push(key.clone());
    }
    // Length now matches; overwrite every slot's content too, not just
    // newly-appended ones (see apply_thread_ops's comment).
    for (index, key) in desired_keys.iter().enumerate() {
        keys[index] = key.clone();
    }
    for (index, row) in desired_rows.into_iter().enumerate() {
        if index < model.messages_model.row_count() {
            model.messages_model.set_row_data(index, row);
        } else {
            model.messages_model.push(row);
        }
    }
}

fn apply_skill_ops(model: &Model, ops: &[RowOp<crate::SkillOption>]) {
    let mut keys = model.skill_model_keys.borrow_mut();
    for op in ops {
        match op {
            RowOp::Insert { at, row } => {
                // See apply_thread_ops's comment: clamp instead of trusting
                // a possibly-stale `at`.
                let at = (*at).min(model.skills_model.row_count()).min(keys.len());
                model.skills_model.insert(at, row.clone());
                keys.insert(at, std::path::PathBuf::from(row.path.to_string()));
            }
            RowOp::Remove { at } => {
                if *at < model.skills_model.row_count() && *at < keys.len() {
                    model.skills_model.remove(*at);
                    keys.remove(*at);
                }
            }
            RowOp::Move { from, to } => {
                if *from < model.skills_model.row_count()
                    && *to <= model.skills_model.row_count()
                    && *from < keys.len()
                    && *to <= keys.len()
                {
                    let row = model.skills_model.remove(*from);
                    model.skills_model.insert(*to, row);
                    let key = keys.remove(*from);
                    keys.insert(*to, key);
                }
            }
        }
    }
    let rows = crate::models::to_skill_option_rows(model.skills.clone());
    // See apply_thread_ops's comment: force convergence to `rows` instead
    // of trusting that the ops above landed the model at the right length.
    let row_paths: Vec<std::path::PathBuf> = model
        .skills
        .iter()
        .map(|skill| skill.path.clone())
        .collect();
    while model.skills_model.row_count() > rows.len() {
        model.skills_model.remove(model.skills_model.row_count() - 1);
    }
    while keys.len() > row_paths.len() {
        keys.pop();
    }
    for path in row_paths.iter().skip(keys.len()) {
        keys.push(path.clone());
    }
    // Length now matches; overwrite every slot's content too (see
    // apply_thread_ops's comment).
    for (index, path) in row_paths.iter().enumerate() {
        keys[index] = path.clone();
    }
    for (index, row) in rows.into_iter().enumerate() {
        if index < model.skills_model.row_count() {
            model.skills_model.set_row_data(index, row);
        } else {
            model.skills_model.push(row);
        }
    }
}

fn reconcile_terminals(model: &Model, terminals: &[crate::TerminalItem]) {
    let new_keys: Vec<String> = terminals
        .iter()
        .map(|term| term.terminal_id.to_string())
        .collect();
    crate::list_model::reconcile(
        &model.terminals_model,
        &mut model.terminal_model_keys.borrow_mut(),
        &new_keys,
        terminals,
    );
}

/// setup-followups plan, provider_fastmode_profile_persistence: refreshes
/// the compose-bar profile picker for `thread` -- its dropdown model
/// (`model.available_profiles`, the same data Settings > Agents' profile
/// chips already fetch), its trigger label (the thread's own
/// `profile_name`, empty until chosen), and whether it's interactive at
/// all (`has-session`: false only until a real ACP session attaches,
/// per ThreadItem.has-session's doc comment).
fn sync_profile_picker(model: &Model, component: &ChatPanel, thread: &crate::model::ThreadModel) {
    let profile_rows = crate::models::to_profile_option_rows(model.available_profiles.clone());
    let current = thread.profile_name.as_deref().unwrap_or("");
    component.set_profile_dropdown_entries(crate::models::to_profile_dropdown_entries(
        &profile_rows,
        current,
    ));
    // Compose trigger shows provider/agent id, not raw profile name.
    component.set_profile_trigger_label(
        crate::models::current_provider_trigger_label(&profile_rows, current).into(),
    );
    component.set_active_thread_has_session(thread.session_id.is_some());
}

/// Refresh model dropdown filtered to the thread's provider.
///
/// `thread_provider_model_binding_fix`: the filter keys off the thread's
/// ACTUAL bound provider (`thread.provider` -- the gateway its live
/// session runs on), not the profile picker's selection. Deriving it
/// from `profile_name` meant selecting a different-provider profile on a
/// live session (which never rebinds the backend -- ACPX has no
/// session/set_profile) instantly re-filtered the model list to a
/// provider the session is NOT running on: the UI showed the new
/// provider's models while the old backend kept serving the thread.
/// The profile-derived agent id remains only as a fallback for a thread
/// that has no provider recorded yet.
fn sync_model_dropdown_for_provider(
    model: &Model,
    component: &ChatPanel,
    thread: &crate::model::ThreadModel,
) {
    let agent_id = if !thread.provider.is_empty() {
        thread.provider.clone()
    } else {
        let profile_rows = crate::models::to_profile_option_rows(model.available_profiles.clone());
        let current = thread.profile_name.as_deref().unwrap_or("");
        crate::models::provider_agent_id_for_profile(&profile_rows, current)
    };
    component.set_config_dropdown_entries(crate::models::to_config_dropdown_entries_for_provider(
        thread.config_options.clone(),
        &agent_id,
    ));
    component.set_config_trigger_label(
        crate::models::current_config_trigger_label(&thread.config_options).into(),
    );
}

fn reconcile_settings_models(model: &Model, component: &ChatPanel) {
    let profile_rows = crate::models::to_profile_option_rows(model.available_profiles.clone());
    let profile_keys: Vec<String> = model
        .available_profiles
        .iter()
        .map(|profile| profile.name.clone())
        .collect();
    crate::list_model::reconcile(
        &model.profiles_model,
        &mut model.profile_model_keys.borrow_mut(),
        &profile_keys,
        &profile_rows,
    );

    let mcp_rows = crate::models::to_mcp_server_option_rows(model.available_mcp_servers.clone());
    let mcp_keys: Vec<String> = model
        .available_mcp_servers
        .iter()
        .map(|server| server.name.clone())
        .collect();
    crate::list_model::reconcile(
        &model.mcp_servers_model,
        &mut model.mcp_server_model_keys.borrow_mut(),
        &mcp_keys,
        &mcp_rows,
    );

    let agent_rows = crate::models::to_agent_catalog_entry_rows(model.agent_catalog.clone());
    let agent_keys: Vec<String> = model
        .agent_catalog
        .iter()
        .map(|agent| agent.id.clone())
        .collect();
    crate::list_model::reconcile(
        &model.agent_catalog_model,
        &mut model.agent_catalog_model_keys.borrow_mut(),
        &agent_keys,
        &agent_rows,
    );

    let session_rows = crate::models::to_remote_session_option_rows(
        model.recoverable_sessions.clone(),
        &model.recovery_provider,
    );
    let session_keys: Vec<String> = model
        .recoverable_sessions
        .iter()
        .map(|session| session.acp_session_id.clone())
        .collect();
    crate::list_model::reconcile(
        &model.recoverable_sessions_model,
        &mut model.recoverable_session_model_keys.borrow_mut(),
        &session_keys,
        &session_rows,
    );

    component.set_available_profiles(slint::ModelRc::from(model.profiles_model.clone()));
    component.set_available_mcp_servers(slint::ModelRc::from(model.mcp_servers_model.clone()));
    component.set_agent_catalog(slint::ModelRc::from(model.agent_catalog_model.clone()));
    component.set_recoverable_sessions(slint::ModelRc::from(
        model.recoverable_sessions_model.clone(),
    ));
}

fn sync_scalar(model: &Model, component: &ChatPanel, field: crate::dirty::ScalarField) {
    use crate::dirty::ScalarField;
    match field {
        ScalarField::SelectedThread => {
            component.set_selected_thread(model.selected_thread as i32);
            // Dirty::Capabilities only fires when session_modes/
            // config_options actually change, which a brand-new empty
            // thread (no capabilities either before or after selecting
            // it) never does -- sync the profile picker here too so its
            // enabled/current state is correct immediately on switching,
            // not just eventually via the next capability event.
            let real_idx = crate::update::selected_real_index(model);
            if let Some(thread) = model.threads.get(real_idx) {
                sync_profile_picker(model, component, thread);
            }
        }
        ScalarField::ComposeText => {
            component.set_compose_text(model.compose_text.clone().into());
        }
        ScalarField::SettingsOpen => {
            component.set_settings_open(model.settings_open);
        }
        ScalarField::SettingsScope => {
            component.set_settings_scope(model.settings_scope.clone().into());
        }
        ScalarField::ExpandedTerminal => {}
        ScalarField::SearchQuery => {}
    }
}

pub(crate) fn sync_geometry(component: &ChatPanel, compact: bool, narrow: bool) {
    component.set_compact(compact);
    component.set_narrow(narrow);
}

pub(crate) fn sync_loading_older(component: &ChatPanel, loading: bool) {
    component.set_loading_older_messages(loading);
}

pub(crate) fn sync_host_appearance(
    component: &ChatPanel,
    appearance: &crate::appearance::HostAppearance,
    theme: &str,
) {
    let theme_global = crate::Theme::get(component);
    theme_global.set_theme(theme.into());
    theme_global.set_host_language_tag(appearance.language_tag.clone().into());
    theme_global.set_host_font_sans(appearance.bundled_font.clone().into());
    theme_global.set_host_font_scale(appearance.font_scale);
    theme_global.set_host_density(appearance.density);
}

fn sync_appearance(model: &Model, component: &ChatPanel) {
    let Some(appearance) = model.appearance.current() else {
        return;
    };
    let theme = if matches!(
        appearance.color_scheme,
        crate::appearance::ColorScheme::Dark
    ) {
        "dark"
    } else {
        "light"
    };
    sync_host_appearance(component, appearance, theme);
}

pub(crate) fn sync_theme_variant(component: &ChatPanel, theme: &str) {
    crate::Theme::get(component).set_theme(theme.into());
}

pub(crate) fn sync_initial_models(model: &Model, component: &ChatPanel) {
    component.set_threads(slint::ModelRc::from(model.thread_model.clone()));
    component.set_messages(slint::ModelRc::from(model.messages_model.clone()));
    component.set_available_skills(slint::ModelRc::from(model.skills_model.clone()));
    component.set_terminals(slint::ModelRc::from(model.terminals_model.clone()));
    reconcile_settings_models(model, component);
}

// No unit tests against a live `ChatPanel` here: constructing one
// requires `slint::platform::set_platform` to have already run (the
// software-renderer platform `panel_rust_create` sets up once per
// process, see `lib.rs`'s `SpikePlatform`/`PLATFORM_WINDOW`) -- confirmed
// live, `ChatPanel::new()` panics ("No default Slint platform was
// selected") in a bare `cargo test` process with no such setup, and nothing
// else in this crate constructs one outside `panel_rust_create` either.
// This matches 00-plan.md's own Phase 3 verification wording ("live
// click-through", not a unit test) -- real coverage for `sync()` lands
// with Phase 4's real-host cutover, not here.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::VisibleThreadItem;
    use std::rc::Rc;

    #[test]
    fn thread_row_ops_apply_to_the_persistent_model_and_key_cache() {
        let mut model = Model::default();
        let row = VisibleThreadItem {
            session_id: None,
            real_index: 7,
            thread_id: "thread-7".to_owned(),
            item: crate::ThreadItem::default(),
        };
        // apply_thread_ops converges thread_model/keys to `model.thread_rows`
        // (the real caller, thread_list_dirty_with_keys, always updates both
        // together) -- a real caller's ops and its `thread_rows` snapshot
        // describe the same target state, so tests must too.
        model.thread_rows.push(row.clone());
        apply_thread_ops(&model, &[RowOp::Insert { at: 0, row }]);
        assert_eq!(model.thread_model.row_count(), 1);
        assert_eq!(*model.thread_model_keys.borrow(), vec!["thread-7"]);

        model.thread_rows.clear();
        apply_thread_ops(&model, &[RowOp::Remove { at: 0 }]);
        assert_eq!(model.thread_model.row_count(), 0);
        assert!(model.thread_model_keys.borrow().is_empty());
    }

    #[test]
    fn thread_row_ops_skip_set_row_data_when_content_is_unchanged() {
        // setup-followups thread_row_time_controls: a no-op force-converge
        // must not re-notify the VecModel (which would recreate if-children
        // and invalidate live MCP element handles). Seed a row, re-apply
        // empty ops against identical thread_rows, and prove the row's
        // content is still the same identity-equivalent value afterwards.
        let mut model = Model::default();
        let item = crate::ThreadItem {
            name: "Fix timeline crash".into(),
            status: "idle".into(),
            open: true,
            ..crate::ThreadItem::default()
        };
        let row = VisibleThreadItem {
            session_id: None,
            real_index: 0,
            thread_id: "thread-0".to_owned(),
            item: item.clone(),
        };
        model.thread_rows.push(row);
        model.thread_model.push(item.clone());
        *model.thread_model_keys.borrow_mut() = vec!["thread-0".to_owned()];

        apply_thread_ops(&model, &[]);

        assert_eq!(model.thread_model.row_count(), 1);
        let after = model.thread_model.row_data(0).unwrap();
        assert_eq!(after.name, item.name);
        assert_eq!(after.status, item.status);
        assert_eq!(after.open, item.open);
        assert_eq!(*model.thread_model_keys.borrow(), vec!["thread-0"]);
    }

    #[test]
    fn thread_row_ops_self_heal_a_stale_key_cache_instead_of_duplicating_rows() {
        // Regression test: a real crash chain hit this exact scenario --
        // `list_model::reconcile`'s (and this file's hand-rolled
        // apply_*_ops's) `keys` cache could desync from the persistent
        // VecModel's real row count. Because this crate is
        // `panic = "abort"`, a bare `assert_eq!` on that invariant used to
        // kill the whole host process on the very next message send after
        // any such desync. Clamping the ops stopped the abort, but on its
        // own that just let a bad diff silently keep *inserting* rows every
        // poll tick forever -- the "clicking + spawns 100s of threads"
        // symptom the abort used to mask. `apply_thread_ops` must converge
        // to `model.thread_rows` (the real source of truth) regardless of
        // how garbled the incoming ops or the pre-existing key cache are.
        let mut model = Model::default();
        // Simulate a badly desynced key cache: three stale keys, but only
        // one live row (mirrors the observed "New thread 3" spam: many
        // stale rows sharing one real thread's identity/content).
        model.thread_model.push(crate::ThreadItem::default());
        model.thread_model.push(crate::ThreadItem::default());
        model.thread_model.push(crate::ThreadItem::default());
        *model.thread_model_keys.borrow_mut() = vec![
            "stale-a".to_owned(),
            "stale-b".to_owned(),
            "stale-c".to_owned(),
        ];
        let row = VisibleThreadItem {
            session_id: None,
            real_index: 0,
            thread_id: "thread-0".to_owned(),
            item: crate::ThreadItem {
                name: "New thread 1".into(),
                ..crate::ThreadItem::default()
            },
        };
        model.thread_rows.push(row.clone());

        // A garbled/stale op list (e.g. an Insert computed against a key
        // cache that no longer matches reality) must not be able to grow
        // the model past the true desired length.
        apply_thread_ops(
            &model,
            &[RowOp::Insert {
                at: 99,
                row: row.clone(),
            }],
        );

        assert_eq!(
            model.thread_model.row_count(),
            1,
            "must converge to the one real thread, not accumulate duplicates"
        );
        assert_eq!(*model.thread_model_keys.borrow(), vec!["thread-0"]);
        assert_eq!(model.thread_model.row_data(0).unwrap().name, "New thread 1");
    }

    #[test]
    fn thread_row_dirty_resolves_against_real_key_after_filtering() {
        let mut model = Model::default();
        model.threads = (0..8)
            .map(|idx| crate::model::ThreadModel {
                thread_id: format!("thread-{idx}"),
                ..crate::model::ThreadModel::default()
            })
            .collect();
        // apply_thread_row recomputes fresh from `model.threads[real_index]`
        // (the live source of truth), not from `model.thread_rows` -- a
        // cache that's only ever refreshed by a full thread-list rebuild
        // (see its own doc comment: this is exactly the "loading doesn't
        // start immediately on send" regression, since Dirty::ThreadRow
        // fires for in-place row changes that a full rebuild hasn't
        // caught up to yet). So the row's *live* display_name is what
        // must change here, not a manually pre-seeded `thread_rows` entry.
        model.threads[7].display_name = "new".to_owned();
        model.thread_model.push(crate::ThreadItem {
            name: "old".into(),
            ..crate::ThreadItem::default()
        });
        *model.thread_model_keys.borrow_mut() = vec!["thread-7".to_owned()];

        apply_thread_row(&model, 7);
        assert_eq!(model.thread_model.row_data(0).unwrap().name, "new");
    }

    #[test]
    fn switching_to_a_brand_new_empty_thread_clears_stale_messages_from_the_shared_model() {
        // setup-followups plan follow-up: closes the loop on update.rs's
        // `switching_to_a_thread_with_a_coincidentally_unchanged_transcript_
        // still_resyncs_the_shared_model` regression test (commit
        // ba6a9d2's "new chat shows prefill data" fix) -- that test only
        // proves the reducer *emits* `Dirty::MessagesDiff`; this one
        // drives the real `update()` -> `apply_message_ops()` pipeline
        // end to end and asserts the shared `messages_model` (what's
        // actually on screen) truly no longer holds thread-0's leftover
        // row after switching to a brand new, empty thread-1.
        let mut model = Model::default();
        model.threads.push(crate::model::ThreadModel {
            thread_id: "thread-0".to_owned(),
            session_id: Some("thread-0-session".to_owned()),
            transcript: vec![crate::conversation::TranscriptItem::Assistant {
                message_id: "old-message".to_owned(),
                text: "leftover from the previous thread".to_owned(),
                streaming: false,
            }],
            transcript_keys: vec!["assistant:old-message".to_owned()],
            ..crate::model::ThreadModel::default()
        });
        model.threads.push(crate::model::ThreadModel {
            thread_id: "thread-1".to_owned(),
            ..crate::model::ThreadModel::default()
        });
        model.displayed_thread = Some(0);
        model.selected_thread = 1;
        // What's actually on screen right now, mirroring thread-0's
        // content -- this is the shared VecModel a stale-data bug would
        // leave untouched.
        model.messages_model.push(crate::MessageItem {
            text: "leftover from the previous thread".into(),
            ..crate::MessageItem::default()
        });
        *model.message_model_keys.borrow_mut() = vec!["assistant:old-message".to_owned()];

        let (_, dirty) = crate::update::update(
            &mut model,
            crate::msg::Msg::Frame(crate::msg::FrameInput {
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
                    usage: (0, 0),
                }),
                ..crate::msg::FrameInput::default()
            }),
        );

        for d in &dirty {
            if let crate::dirty::Dirty::MessagesDiff { thread_id, ops } = d {
                apply_message_ops(&model, thread_id, ops);
            }
        }

        assert_eq!(
            model.messages_model.row_count(),
            0,
            "expected thread-1 (brand new, no messages) to leave the shared messages_model \
             empty, but thread-0's leftover row is still there"
        );
        assert!(
            model.message_model_keys.borrow().is_empty(),
            "message_model_keys must converge to empty alongside the model"
        );
    }

    #[test]
    fn thread_row_updates_when_key_cache_has_synthetic_id_but_model_has_durable_id() {
        // setup-followups thread_view_items_not_updating_ui: cold-start
        // keys use "thread:N" while model.threads later get a durable
        // bridge session id — pure durable-id lookup used to no-op.
        let mut model = Model::default();
        model.threads.push(crate::model::ThreadModel {
            thread_id: "durable-uuid-0".to_owned(),
            display_name: "Fix timeline crash".to_owned(),
            state: crate::models::ThreadState::Idle,
            ..crate::model::ThreadModel::default()
        });
        model.thread_model.push(crate::ThreadItem {
            name: "Fix timeline crash".into(),
            status: "idle".into(),
            busy: false,
            description: "last reply preview".into(),
            ..crate::ThreadItem::default()
        });
        *model.thread_model_keys.borrow_mut() = vec!["thread:0".to_owned()];
        model.visible_indices = vec![0];
        model.thread_rows.push(VisibleThreadItem {
            session_id: None,
            real_index: 0,
            thread_id: "thread:0".to_owned(),
            item: crate::ThreadItem {
                name: "Fix timeline crash".into(),
                status: "idle".into(),
                description: "last reply preview".into(),
                ..crate::ThreadItem::default()
            },
        });

        model.threads[0].state = crate::models::ThreadState::Loading;
        apply_thread_row(&model, 0);

        let row = model.thread_model.row_data(0).unwrap();
        assert_eq!(row.status, "loading");
        assert!(row.busy);
        assert_eq!(
            row.description, "last reply preview",
            "display fields from the snapshot must survive a status-only ThreadRow"
        );
        assert_eq!(
            model.thread_model_keys.borrow()[0],
            "durable-uuid-0",
            "key cache must converge to the durable id after a successful apply"
        );
    }

    #[test]
    fn thread_row_reflects_a_just_started_loading_state_even_with_a_stale_thread_rows_cache() {
        // Regression test: "loading should start immediately on send".
        // Models the exact real sequence -- ComposeMsg::SendRequested
        // flips `model.threads[idx].state` to `Loading` and returns
        // `Dirty::ThreadRow(idx)` in the *same* reducer call, before
        // anything has refreshed `model.thread_rows` (that only happens
        // on a full thread-list rebuild, e.g. create/select/close). If
        // apply_thread_row trusted that stale cache instead of the live
        // `model.threads` state, the sidebar spinner and the chat area's
        // `sending`-derived pulse would both stay frozen on "idle" until
        // some unrelated later event forced a full rebuild.
        let mut model = Model::default();
        model.threads.push(crate::model::ThreadModel {
            thread_id: "thread-0".to_owned(),
            state: crate::models::ThreadState::Idle,
            ..crate::model::ThreadModel::default()
        });
        model.thread_model.push(crate::ThreadItem {
            status: "idle".into(),
            busy: false,
            ..crate::ThreadItem::default()
        });
        *model.thread_model_keys.borrow_mut() = vec!["thread-0".to_owned()];
        // thread_rows deliberately left empty/stale -- nothing has
        // rebuilt it since cold start, same as a real live app between
        // list rebuilds.
        assert!(model.thread_rows.is_empty());

        model.threads[0].state = crate::models::ThreadState::Loading;
        apply_thread_row(&model, 0);

        let row = model.thread_model.row_data(0).unwrap();
        assert_eq!(row.status, "loading");
        assert!(row.busy, "the sidebar spinner's busy flag must flip immediately, not on the \
                           next unrelated thread-list rebuild");
    }

    #[test]
    fn message_streaming_delta_resolves_by_stable_id_not_position() {
        let mut model = Model::default();
        model.threads.push(crate::model::ThreadModel {
            session_id: Some("thread-1".to_owned()),
            ..crate::model::ThreadModel::default()
        });
        model.displayed_thread = Some(0);
        model.messages_model.push(crate::MessageItem {
            text: "hello".into(),
            ..crate::MessageItem::default()
        });
        *model.message_model_keys.borrow_mut() = vec!["assistant:m-1".to_owned()];
        apply_message_streaming(&model, "thread-1", "m-1", " world");
        assert_eq!(
            model.messages_model.row_data(0).unwrap().text,
            "hello world"
        );
    }

    #[test]
    fn message_streaming_delta_resolves_by_durable_thread_id() {
        let mut model = Model::default();
        model.threads.push(crate::model::ThreadModel {
            thread_id: "durable-thread-1".to_owned(),
            ..crate::model::ThreadModel::default()
        });
        model.displayed_thread = Some(0);
        model.messages_model.push(crate::MessageItem {
            text: "hello".into(),
            ..crate::MessageItem::default()
        });
        *model.message_model_keys.borrow_mut() = vec!["assistant:m-1".to_owned()];

        apply_message_streaming(&model, "durable-thread-1", "m-1", " world");

        assert_eq!(
            model.messages_model.row_data(0).unwrap().text,
            "hello world"
        );
    }

    #[test]
    fn message_streaming_delta_for_background_thread_is_ignored() {
        let mut model = Model::default();
        model.threads.extend([
            crate::model::ThreadModel {
                session_id: Some("displayed-thread".to_owned()),
                ..crate::model::ThreadModel::default()
            },
            crate::model::ThreadModel {
                session_id: Some("background-thread".to_owned()),
                ..crate::model::ThreadModel::default()
            },
        ]);
        model.displayed_thread = Some(0);
        model.messages_model.push(crate::MessageItem {
            text: "displayed".into(),
            ..crate::MessageItem::default()
        });
        *model.message_model_keys.borrow_mut() = vec!["assistant:shared-id".to_owned()];

        apply_message_streaming(&model, "background-thread", "shared-id", " must not appear");

        assert_eq!(model.messages_model.row_data(0).unwrap().text, "displayed");
    }

    #[test]
    fn terminals_reconcile_in_place_without_replacing_the_model() {
        let model = Model::default();
        model.terminals_model.push(crate::TerminalItem {
            terminal_id: "t1".into(),
            output: "old".into(),
            ..crate::TerminalItem::default()
        });
        *model.terminal_model_keys.borrow_mut() = vec!["t1".to_owned()];
        let identity = model.terminals_model.clone();

        reconcile_terminals(
            &model,
            &[
                crate::TerminalItem {
                    terminal_id: "t1".into(),
                    output: "new".into(),
                    ..crate::TerminalItem::default()
                },
                crate::TerminalItem {
                    terminal_id: "t2".into(),
                    output: "added".into(),
                    ..crate::TerminalItem::default()
                },
            ],
        );

        assert!(Rc::ptr_eq(&identity, &model.terminals_model));
        assert_eq!(model.terminals_model.row_count(), 2);
        assert_eq!(model.terminals_model.row_data(0).unwrap().output, "new");
        assert_eq!(
            *model.terminal_model_keys.borrow(),
            vec!["t1".to_owned(), "t2".to_owned()]
        );
    }

    #[test]
    fn message_snapshot_reconciles_rows_without_replacing_the_model() {
        let mut model = Model::default();
        model.threads.push(crate::model::ThreadModel {
            session_id: Some("thread-1".to_owned()),
            transcript_keys: vec!["assistant:m-1".to_owned()],
            message_rows: vec![crate::MessageItem {
                text: "updated".into(),
                ..crate::MessageItem::default()
            }],
            ..crate::model::ThreadModel::default()
        });
        model.displayed_thread = Some(0);
        model.messages_model.push(crate::MessageItem {
            text: "old".into(),
            ..crate::MessageItem::default()
        });
        *model.message_model_keys.borrow_mut() = vec!["assistant:m-1".to_owned()];
        let model_identity = model.messages_model.clone();

        sync_message_snapshot(&model, "thread-1");

        assert!(std::rc::Rc::ptr_eq(&model_identity, &model.messages_model));
        assert_eq!(*model.message_model_keys.borrow(), vec!["assistant:m-1"]);
        assert_eq!(model.messages_model.row_data(0).unwrap().text, "updated");
    }
}
