//! `tea-slint-model` Phase 3: `sync(&Model, &ChatPanel, &[Dirty])` -- the
//! **sole** owner of pushing `Model` state into Slint `set_*` setters.
//! See `memory/rui/gen/plans/tea-slint-model/00-plan.md`.
//!
//! **Status: partially live.** Dispatchers and cold-start hydration call
//! `sync()` for their returned Dirty markers. Persistent row operations and
//! error projection are live; bridge-backed messages, terminals, requests,
//! capabilities, and settings gateway snapshots still use the legacy refresh
//! cascade until those values are Model-owned.
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

fn sync_one(model: &Model, component: &ChatPanel, dirty: &Dirty) {
    match dirty {
        Dirty::Scalar(field) => sync_scalar(model, component, *field),
        Dirty::ThreadRow(idx) => {
            apply_thread_row(model, *idx);
        }
        Dirty::ThreadListDiff(ops) => apply_thread_ops(model, ops),
        Dirty::MessageAppended { thread_id } => {
            sync_message_snapshot(model, thread_id);
            sync_has_older_messages(model, component, thread_id);
        }
        Dirty::MessagesDiff { thread_id, ops } => {
            apply_message_ops(model, thread_id, ops);
            sync_has_older_messages(model, component, thread_id);
        }
        Dirty::MessageStreamingDelta {
            thread_id,
            message_id,
            delta,
        } => {
            apply_message_streaming(model, message_id, delta);
            sync_has_older_messages(model, component, thread_id);
        }
        Dirty::Connection { thread_id } => {
            if let Some(thread) = thread_for_id(model, thread_id) {
                component.set_connection_status(thread.connection_status.clone().into());
            }
        }
        Dirty::Error { thread_id, detail } => {
            let displayed_thread_id = model
                .displayed_thread
                .and_then(|idx| model.threads.get(idx))
                .and_then(|thread| thread.session_id.as_deref());
            if thread_id.is_empty() || displayed_thread_id == Some(thread_id.as_str()) {
                component.set_last_error(detail.message.clone().into());
            }
        }
        Dirty::PendingRequest { thread_id } => {
            if let Some(thread) = thread_for_id(model, thread_id) {
                component.set_pending_request(thread.pending_request.clone());
            }
        }
        Dirty::Terminal { .. } => {
            if let Some(idx) = model.displayed_thread {
                if let Some(thread) = model.threads.get(idx) {
                    component.set_terminals(slint::ModelRc::new(slint::VecModel::from(
                        thread.terminals.clone(),
                    )));
                    if let Some(expanded) = &thread.expanded_terminal {
                        component.set_expanded_terminal(expanded.clone());
                    } else {
                        component.set_expanded_terminal(crate::TerminalItem::default());
                    }
                }
            }
        }
        Dirty::LocalTerminal => {
            if let Some(idx) = model.displayed_thread {
                if let Some(thread) = model.threads.get(idx) {
                    component.set_local_terminal(thread.local_terminal.clone());
                }
            }
        }
        Dirty::ProjectPath => {
            component.set_active_project_path(
                model.active_project_path.clone().unwrap_or_default().into(),
            );
        }
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
        Dirty::Capabilities { thread_id } => {
            if let Some(thread) = thread_for_id(model, thread_id) {
                component.set_mode_trigger_label(
                    crate::models::current_mode_name(&thread.session_modes).into(),
                );
                component.set_mode_dropdown_entries(crate::models::to_mode_dropdown_entries(
                    thread.session_modes.clone(),
                ));
                component.set_config_trigger_label(
                    crate::models::current_config_trigger_label(&thread.config_options).into(),
                );
                component.set_config_dropdown_entries(crate::models::to_config_dropdown_entries(
                    thread.config_options.clone(),
                ));
            }
        }
    }
}

fn apply_thread_row(model: &Model, real_index: usize) {
    let keys = model.thread_model_keys.borrow();
    let Some(thread_id) = model
        .threads
        .get(real_index)
        .map(|thread| &thread.thread_id)
    else {
        return;
    };
    let Some(row_index) = keys.iter().position(|key| key == thread_id) else {
        return;
    };
    if let Some(row) = model.thread_rows.get(row_index) {
        model.thread_model.set_row_data(row_index, row.item.clone());
    }
}

fn thread_for_id<'a>(model: &'a Model, thread_id: &str) -> Option<&'a crate::model::ThreadModel> {
    model
        .threads
        .iter()
        .find(|thread| thread_id.is_empty() || thread.session_id.as_deref() == Some(thread_id))
}

fn displayed_thread_for_id(model: &Model, thread_id: &str) -> Option<usize> {
    model.displayed_thread.filter(|idx| {
        model
            .threads
            .get(*idx)
            .and_then(|thread| thread.session_id.as_deref())
            .is_some_and(|id| thread_id.is_empty() || id == thread_id)
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

fn apply_message_streaming(model: &Model, message_id: &str, delta: &str) {
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
                model.thread_model.insert(*at, row.item.clone());
                keys.insert(*at, row.thread_id.clone());
            }
            RowOp::Remove { at } => {
                if *at < model.thread_model.row_count() {
                    model.thread_model.remove(*at);
                    keys.remove(*at);
                }
            }
            RowOp::Move { from, to } => {
                if *from < model.thread_model.row_count() && *to <= model.thread_model.row_count() {
                    let row = model.thread_model.remove(*from);
                    model.thread_model.insert(*to, row);
                    let key = keys.remove(*from);
                    keys.insert(*to, key);
                }
            }
        }
    }
    for (index, row) in model.thread_rows.iter().enumerate() {
        if index < model.thread_model.row_count() {
            model.thread_model.set_row_data(index, row.item.clone());
        }
    }
}

fn apply_message_ops(model: &Model, thread_id: &str, ops: &[RowOp<crate::MessageItem>]) {
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
                model.messages_model.insert(*at, row.clone());
                keys.insert(*at, desired_keys.get(*at).cloned().unwrap_or_default());
            }
            RowOp::Remove { at } => {
                if *at < model.messages_model.row_count() {
                    model.messages_model.remove(*at);
                    keys.remove(*at);
                }
            }
            RowOp::Move { from, to } => {
                if *from < model.messages_model.row_count()
                    && *to <= model.messages_model.row_count()
                {
                    let row = model.messages_model.remove(*from);
                    model.messages_model.insert(*to, row);
                    let key = keys.remove(*from);
                    keys.insert(*to, key);
                }
            }
        }
    }
    for (index, row) in desired_rows.into_iter().enumerate() {
        if index < model.messages_model.row_count() {
            model.messages_model.set_row_data(index, row);
        }
    }
}

fn apply_skill_ops(model: &Model, ops: &[RowOp<crate::SkillOption>]) {
    let mut keys = model.skill_model_keys.borrow_mut();
    for op in ops {
        match op {
            RowOp::Insert { at, row } => {
                model.skills_model.insert(*at, row.clone());
                keys.insert(*at, std::path::PathBuf::from(row.path.to_string()));
            }
            RowOp::Remove { at } => {
                if *at < model.skills_model.row_count() {
                    model.skills_model.remove(*at);
                    keys.remove(*at);
                }
            }
            RowOp::Move { from, to } => {
                if *from < model.skills_model.row_count() && *to <= model.skills_model.row_count() {
                    let row = model.skills_model.remove(*from);
                    model.skills_model.insert(*to, row);
                    let key = keys.remove(*from);
                    keys.insert(*to, key);
                }
            }
        }
    }
    let rows = crate::models::to_skill_option_rows(model.skills.clone());
    for (index, row) in rows.into_iter().enumerate() {
        if index < model.skills_model.row_count() {
            model.skills_model.set_row_data(index, row);
        }
    }
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

pub(crate) fn sync_skill_editor(
    component: &ChatPanel,
    name: slint::SharedString,
    path: slint::SharedString,
    content: slint::SharedString,
    detected_editors: slint::ModelRc<slint::SharedString>,
) {
    component.set_active_skill_name(name);
    component.set_active_skill_path(path);
    component.set_active_skill_content(content);
    component.set_detected_editors(detected_editors);
    component.set_active_pane("skill".into());
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

pub(crate) fn sync_theme_variant(component: &ChatPanel, theme: &str) {
    crate::Theme::get(component).set_theme(theme.into());
}

pub(crate) fn sync_initial_models(model: &Model, component: &ChatPanel) {
    component.set_threads(slint::ModelRc::from(model.thread_model.clone()));
    component.set_messages(slint::ModelRc::from(model.messages_model.clone()));
    component.set_available_skills(slint::ModelRc::from(model.skills_model.clone()));
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

    #[test]
    fn thread_row_ops_apply_to_the_persistent_model_and_key_cache() {
        let model = Model::default();
        apply_thread_ops(
            &model,
            &[RowOp::Insert {
                at: 0,
                row: VisibleThreadItem {
                    real_index: 7,
                    thread_id: "thread-7".to_owned(),
                    item: crate::ThreadItem::default(),
                },
            }],
        );
        assert_eq!(model.thread_model.row_count(), 1);
        assert_eq!(*model.thread_model_keys.borrow(), vec!["thread-7"]);

        apply_thread_ops(&model, &[RowOp::Remove { at: 0 }]);
        assert_eq!(model.thread_model.row_count(), 0);
        assert!(model.thread_model_keys.borrow().is_empty());
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
        model.thread_model.push(crate::ThreadItem {
            name: "old".into(),
            ..crate::ThreadItem::default()
        });
        *model.thread_model_keys.borrow_mut() = vec!["thread-7".to_owned()];
        let row = crate::models::VisibleThreadItem {
            real_index: 7,
            thread_id: "thread-7".to_owned(),
            item: crate::ThreadItem {
                name: "new".into(),
                ..crate::ThreadItem::default()
            },
        };
        model.thread_rows.push(row.clone());

        apply_thread_row(&model, 7);
        assert_eq!(model.thread_model.row_data(0).unwrap().name, "new");
    }

    #[test]
    fn message_streaming_delta_resolves_by_stable_id_not_position() {
        let model = Model::default();
        model.messages_model.push(crate::MessageItem {
            text: "hello".into(),
            ..crate::MessageItem::default()
        });
        *model.message_model_keys.borrow_mut() = vec!["assistant:m-1".to_owned()];
        apply_message_streaming(&model, "m-1", " world");
        assert_eq!(
            model.messages_model.row_data(0).unwrap().text,
            "hello world"
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
