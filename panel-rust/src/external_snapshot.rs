//! External effect-source boundary for the TEA frame loop.
//!
//! This module only reads bridge/store/watcher state and packages it into a
//! `FrameInput`. It never mutates `Model` or calls a Slint setter. The
//! reducer remains responsible for folding the snapshot, and `sync()` remains
//! responsible for presentation.

use crate::{models, msg, AgentBridge, PanelSingleton};
use slint::ModelRc;
use std::sync::atomic::Ordering;

pub(crate) struct ExternalSnapshotSource<'a> {
    panel: &'a PanelSingleton,
}

impl<'a> ExternalSnapshotSource<'a> {
    pub(crate) fn new(panel: &'a PanelSingleton) -> Self {
        Self { panel }
    }

    pub(crate) fn collect_frame_input(&self) -> msg::FrameInput {
        let bridge_events = self
            .panel
            .bridge
            .as_ref()
            .map(AgentBridge::poll)
            .unwrap_or_default();
        let bridge_event_thread_ids = bridge_events
            .iter()
            .map(|event| {
                self.panel
                    .bridge
                    .as_ref()
                    .and_then(|bridge| bridge.thread_binding(event.thread_index))
                    .map(|binding| binding.thread_id)
                    .or_else(|| {
                        self.panel
                            .model
                            .borrow()
                            .threads
                            .get(event.thread_index)
                            .map(|thread| thread.thread_id.clone())
                    })
                    .unwrap_or_default()
            })
            .collect();
        let thread_record_snapshots = self.collect_thread_record_snapshots();
        let settings_reload_pending = self
            .panel
            .settings_reload_pending
            .swap(false, Ordering::SeqCst)
            && !self
                .panel
                .settings_ignore_watch_until
                .get()
                .is_some_and(|until| std::time::Instant::now() < until);

        msg::FrameInput {
            bridge_events_pending: !bridge_events.is_empty(),
            bridge_events,
            bridge_event_thread_ids,
            thread_record_snapshots,
            settings_reload_pending,
            prepend_expanded_rows: 0,
            clear_selected_thread: false,
            thread_list_snapshot: Some(self.collect_thread_list_snapshot()),
            selected_thread_snapshot: self.collect_selected_thread_snapshot(),
            settings_preferences_snapshot: (self.panel.model.borrow().settings_open
                || settings_reload_pending)
                .then(|| self.collect_settings_preferences_snapshot(None)),
            settings_gateway_snapshot: self
                .panel
                .model
                .borrow()
                .settings_open
                .then(|| self.collect_settings_gateway_snapshot()),
            skills_snapshot: None,
        }
    }

    pub(crate) fn collect_settings_gateway_snapshot(&self) -> msg::SettingsGatewaySnapshot {
        self.panel
            .bridge
            .as_ref()
            .map(|bridge| {
                let gw = self.panel.settings_gateway_index();
                msg::SettingsGatewaySnapshot {
                    profiles: bridge.list_profiles(gw),
                    mcp_servers: bridge.list_mcp_servers(gw),
                    agents: bridge.list_agents(gw),
                    recoverable_sessions: bridge.recoverable_sessions(gw),
                    recovery_provider: bridge.thread_provider(gw).unwrap_or_default(),
                }
            })
            .unwrap_or(msg::SettingsGatewaySnapshot {
                profiles: Vec::new(),
                mcp_servers: Vec::new(),
                agents: Vec::new(),
                recoverable_sessions: Vec::new(),
                recovery_provider: String::new(),
            })
    }

    pub(crate) fn collect_settings_preferences_snapshot(
        &self,
        requested_scope: Option<&str>,
    ) -> msg::SettingsPreferencesSnapshot {
        let model_scope = self.panel.model.borrow().settings_scope.clone();
        let default_scope = if crate::settings_file::SettingsPaths::from_env()
            .project
            .is_some()
        {
            "project"
        } else {
            "global"
        };
        let scope = requested_scope
            .filter(|scope| *scope == "global" || *scope == "project")
            .or_else(|| (!model_scope.is_empty()).then_some(model_scope.as_str()))
            .unwrap_or(default_scope);
        let selected_thread_id = self
            .panel
            .real_index(self.panel.model.borrow().selected_thread)
            .and_then(|idx| {
                self.panel
                    .bridge
                    .as_ref()
                    .and_then(|bridge| bridge.thread_binding(idx))
                    .map(|binding| binding.thread_id)
            });
        let prefs = crate::load_scoped_panel_prefs(scope, selected_thread_id.clone());
        let (defaults, default_agent_id) = prefs
            .map(|prefs| (prefs.defaults, prefs.default_agent_id))
            .unwrap_or_else(|| {
                let defaults = crate::load_panel_prefs(selected_thread_id.clone());
                let default_agent_id = crate::settings_file::SettingsPaths::from_env()
                    .load_resolved()
                    .ok()
                    .and_then(|resolved| resolved.default_agent_id);
                (defaults, default_agent_id)
            });
        let (background_override_set, background_override) = selected_thread_id
            .as_deref()
            .and_then(|thread_id| {
                self.panel
                    .panel_state
                    .as_ref()
                    .and_then(|store| store.thread_settings(thread_id).ok().flatten())
                    .and_then(|settings| settings.background_session)
                    .map(|value| (true, value))
            })
            .unwrap_or((false, false));
        msg::SettingsPreferencesSnapshot {
            scope: scope.to_owned(),
            default_profile: defaults.profile_name.unwrap_or_default(),
            permission_profile: defaults.permission_profile.unwrap_or_default(),
            background_default: defaults.background_session,
            default_agent_id: default_agent_id.unwrap_or_default(),
            dev_mode: crate::settings_file::SettingsPaths::from_env().dev_mode(),
            background_override_set,
            background_override,
        }
    }

    pub(crate) fn collect_thread_list_snapshot(&self) -> msg::ThreadListSnapshot {
        let model = self.panel.model.borrow();
        let state: Vec<models::ThreadState> = model
            .threads
            .iter()
            .map(|thread| thread.state.clone())
            .collect();
        let query = model.search_query.clone();
        let names: Vec<String> = model
            .threads
            .iter()
            .map(|thread| thread.display_name.clone())
            .collect();
        let thread_ids: Vec<String> = model
            .threads
            .iter()
            .enumerate()
            .map(|(idx, thread)| {
                self.panel
                    .bridge
                    .as_ref()
                    .and_then(|bridge| bridge.thread_binding(idx))
                    .map(|binding| binding.thread_id)
                    .unwrap_or_else(|| {
                        if thread.thread_id.is_empty() {
                            format!("thread:{idx}")
                        } else {
                            thread.thread_id.clone()
                        }
                    })
            })
            .collect();
        let errors: Vec<String> = model
            .threads
            .iter()
            .map(|thread| thread.error.clone().unwrap_or_default())
            .collect();
        drop(model);

        let descriptions: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(idx, _)| {
                if let Some(error) = errors.get(idx).filter(|error| !error.is_empty()) {
                    return format!("Error: {error}");
                }
                self.panel
                    .bridge
                    .as_ref()
                    .map(|bridge| {
                        models::describe_thread(
                            &bridge.history(idx),
                            crate::THREAD_DESCRIPTION_MAX_CHARS,
                        )
                    })
                    .unwrap_or_default()
            })
            .collect();
        let background_sessions: Vec<bool> = names
            .iter()
            .enumerate()
            .map(|(idx, _)| {
                let Some(store) = self.panel.panel_state.as_ref() else {
                    return false;
                };
                let Some(thread_id) = self
                    .panel
                    .bridge
                    .as_ref()
                    .and_then(|bridge| bridge.thread_binding(idx))
                    .map(|binding| binding.thread_id)
                else {
                    return false;
                };
                store
                    .effective_background_session(&thread_id)
                    .unwrap_or(false)
            })
            .collect();
        let closed: Vec<bool> = names
            .iter()
            .enumerate()
            .map(|(idx, _)| {
                self.panel
                    .bridge
                    .as_ref()
                    .map(|bridge| bridge.thread_closed(idx))
                    .unwrap_or(false)
            })
            .collect();
        // setup-followups plan, archive_thread_backend_verify: re-homed
        // here from the pre-TEA `refresh_threads_model` this module
        // replaced -- see AgentBridge::thread_archived's doc comment.
        let archived: Vec<bool> = names
            .iter()
            .enumerate()
            .map(|(idx, _)| {
                self.panel
                    .bridge
                    .as_ref()
                    .map(|bridge| bridge.thread_archived(idx))
                    .unwrap_or(false)
            })
            .collect();
        let providers: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(idx, _)| {
                self.panel
                    .bridge
                    .as_ref()
                    .and_then(|bridge| bridge.thread_provider(idx))
                    .unwrap_or_default()
            })
            .collect();
        let thread_models: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(idx, _)| {
                self.panel
                    .bridge
                    .as_ref()
                    .map(|bridge| models::model_name_from_config(&bridge.config_options(idx)))
                    .unwrap_or_default()
            })
            .collect();
        let thread_project_paths: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(idx, _)| {
                self.panel
                    .bridge
                    .as_ref()
                    .and_then(|bridge| bridge.thread_project_path(idx))
                    .unwrap_or_default()
            })
            .collect();
        let items = models::build_thread_items(
            &names,
            &state,
            &descriptions,
            &background_sessions,
            &closed,
            &archived,
            &query,
        );
        let visible_indices: Vec<usize> = items.iter().map(|item| item.real_index).collect();
        // Profile/session identity live on the TEA ThreadModel, not the
        // name/state slices build_thread_items filters -- post-populate
        // them the same way provider/model/project are, so a frame poll
        // does not rewrite every row with has_session=false /
        // profile_name="" and force a ThreadListDiff + set_row_data
        // every tick (which tears down sidebar if-conditional children
        // such as the close/delete IconButtons and invalidates live MCP
        // element handles).
        let model_snapshot = self.panel.model.borrow();
        let rows: Vec<models::VisibleThreadItem> = items
            .into_iter()
            .map(|item| {
                let mut row = item.item;
                row.provider = providers
                    .get(item.real_index)
                    .cloned()
                    .unwrap_or_default()
                    .into();
                row.model = thread_models
                    .get(item.real_index)
                    .cloned()
                    .unwrap_or_default()
                    .into();
                let project_path = thread_project_paths
                    .get(item.real_index)
                    .cloned()
                    .unwrap_or_default();
                row.project_name = std::path::Path::new(&project_path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .into();
                row.project_path = project_path.into();
                if let Some(thread) = model_snapshot.threads.get(item.real_index) {
                    row.profile_name = thread.profile_name.clone().unwrap_or_default().into();
                    row.has_session = thread.session_id.is_some();
                }
                if !row.closed
                    && self
                        .panel
                        .bridge
                        .as_ref()
                        .is_some_and(|bridge| bridge.thread_binding(item.real_index).is_none())
                {
                    row.status = "loading".into();
                    row.busy = true;
                }
                models::VisibleThreadItem {
                    real_index: item.real_index,
                    thread_id: thread_ids
                        .get(item.real_index)
                        .cloned()
                        .unwrap_or_else(|| format!("thread:{}", item.real_index)),
                    item: row,
                }
            })
            .collect();
        drop(model_snapshot);
        msg::ThreadListSnapshot {
            visible_indices,
            visible_thread_ids: rows.iter().map(|row| row.thread_id.clone()).collect(),
            rows,
        }
    }

    pub(crate) fn collect_skills_snapshot(&self) -> Vec<crate::skills_state::SkillEntry> {
        let global_dir = crate::skills_state::global_skills_dir(&crate::resolve_cache_dir());
        let mut entries = crate::skills_state::scan_skills_dir(
            &global_dir,
            crate::skills_state::SkillScope::Global,
        );
        let active_project_path = self.panel.model.borrow().active_project_path.clone();
        if let Some(project_path) = active_project_path.as_ref() {
            if let Some(project_dir) = std::path::Path::new(project_path).parent() {
                entries.extend(crate::skills_state::scan_skills_dir(
                    &crate::skills_state::project_skills_dir(project_dir),
                    crate::skills_state::SkillScope::Project,
                ));
            }
        }
        entries
    }

    pub(crate) fn collect_thread_snapshot_for(
        &self,
        real_idx: usize,
    ) -> Option<msg::ThreadFrameSnapshot> {
        let bridge = self.panel.bridge.as_ref()?;
        let pending_request = match bridge.pending_requests(real_idx).first() {
            Some(event) => {
                let view = crate::permission::to_pending_request_view(event);
                crate::PendingRequestItem {
                    active: true,
                    relay_id: view.relay_id.into(),
                    method: view.method.into(),
                    title: view.title.into(),
                    summary: view.summary.into(),
                    supported: crate::permission::is_supported_method(&event.method),
                    options: ModelRc::new(slint::VecModel::from(
                        crate::permission::to_permission_option_rows(view.options),
                    )),
                }
            }
            None => crate::PendingRequestItem::default(),
        };
        let terminal_ids = bridge.active_terminals(real_idx);
        let terminals = terminal_ids
            .iter()
            .map(|id| (id.clone(), bridge.terminal_buffer(real_idx, id)))
            .collect();
        let expanded_id = self.panel.model.borrow().expanded_terminal_id.clone();
        let expanded_terminal = expanded_id.and_then(|id| {
            bridge
                .terminal_buffer(real_idx, &id)
                .map(|buffer| crate::TerminalItem {
                    terminal_id: id.into(),
                    output: buffer.output.into(),
                    truncated: buffer.truncated,
                    has_exited: buffer.exit_status.is_some(),
                    exit_code: buffer
                        .exit_status
                        .and_then(|(code, _signal)| code)
                        .unwrap_or_default(),
                })
        });
        Some(msg::ThreadFrameSnapshot {
            thread_id: bridge
                .thread_binding(real_idx)
                .map(|binding| binding.thread_id)
                .or_else(|| {
                    self.panel
                        .model
                        .borrow()
                        .threads
                        .get(real_idx)
                        .map(|thread| thread.thread_id.clone())
                })
                .unwrap_or_else(|| format!("thread:{real_idx}")),
            real_index: real_idx,
            transcript: bridge.transcript(real_idx),
            has_older_messages: bridge.has_older_page(real_idx),
            pending_request,
            terminals: crate::models::to_terminal_item_rows(terminals),
            expanded_terminal,
            local_terminal: crate::models::to_local_terminal_item(
                bridge.local_terminal_snapshot(real_idx),
            ),
            connection_status: bridge.transport_status(real_idx),
            session_modes: bridge.session_modes(real_idx),
            config_options: bridge.config_options(real_idx),
        })
    }

    pub(crate) fn collect_selected_thread_snapshot(&self) -> Option<msg::ThreadFrameSnapshot> {
        let selected = self
            .panel
            .real_index(self.panel.model.borrow().selected_thread);
        selected.and_then(|real_idx| self.collect_thread_snapshot_for(real_idx))
    }

    pub(crate) fn collect_thread_record_snapshots(&self) -> Vec<crate::state_store::ThreadRecord> {
        let Some(bridge) = self.panel.bridge.as_ref() else {
            return Vec::new();
        };
        let model = self.panel.model.borrow();
        model
            .threads
            .iter()
            .enumerate()
            .filter_map(|(idx, thread)| {
                let binding = bridge.thread_binding(idx)?;
                let provider = bridge.thread_provider(idx)?;
                Some(crate::state_store::ThreadRecord {
                    thread_id: binding.thread_id,
                    display_name: thread.display_name.clone(),
                    provider,
                    session_id: binding.session_id,
                    profile_name: thread.profile_name.clone(),
                    permission_profile: thread.permission_profile.clone(),
                    background_session: None,
                })
            })
            .collect()
    }

    pub(crate) fn collect_thread_record(
        &self,
        real_idx: usize,
    ) -> Option<crate::state_store::ThreadRecord> {
        let bridge = self.panel.bridge.as_ref()?;
        let thread = self.panel.model.borrow().threads.get(real_idx)?.clone();
        let binding = bridge.thread_binding(real_idx)?;
        let provider = bridge.thread_provider(real_idx)?;
        Some(crate::state_store::ThreadRecord {
            thread_id: binding.thread_id,
            display_name: thread.display_name,
            provider,
            session_id: binding.session_id,
            profile_name: thread.profile_name,
            permission_profile: thread.permission_profile,
            background_session: None,
        })
    }
}
