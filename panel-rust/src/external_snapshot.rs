//! External effect-source boundary for the TEA frame loop.
//!
//! This module only reads bridge/store/watcher state and packages it into a
//! `FrameInput`. It never mutates `Model` or calls a Slint setter. The
//! reducer remains responsible for folding the snapshot, and `sync()` remains
//! responsible for presentation.

use crate::{models, msg, AgentBridge, PanelSingleton};
use slint::ModelRc;
use std::sync::atomic::Ordering;

/// Phase 27: ~1s throttle for the settings-open skills re-scan (the frame
/// poll runs per repaint; scanning the skills directories that often is
/// pure waste). Thread-local because the poll only ever runs on the UI
/// thread.
fn skills_rescan_due() -> bool {
    thread_local! {
        static LAST_SKILLS_SCAN: std::cell::Cell<Option<std::time::Instant>> =
            const { std::cell::Cell::new(None) };
    }
    LAST_SKILLS_SCAN.with(|last| {
        let now = std::time::Instant::now();
        let due = last
            .get()
            .is_none_or(|at| now.duration_since(at) >= std::time::Duration::from_secs(1));
        if due {
            last.set(Some(now));
        }
        due
    })
}

/// Throttle for settings prefs disk/SQLite reads on the UI frame path
/// (lock_audit F-04). Returns true when a full rescan should run.
fn prefs_rescan_due() -> bool {
    thread_local! {
        static LAST_PREFS_SCAN: std::cell::Cell<Option<std::time::Instant>> =
            const { std::cell::Cell::new(None) };
    }
    LAST_PREFS_SCAN.with(|last| {
        let now = std::time::Instant::now();
        let due = last
            .get()
            .is_none_or(|at| now.duration_since(at) >= std::time::Duration::from_secs(1));
        if due {
            last.set(Some(now));
        }
        due
    })
}

thread_local! {
    static LAST_PREFS_SNAPSHOT: std::cell::RefCell<Option<(String, msg::SettingsPreferencesSnapshot)>> =
        const { std::cell::RefCell::new(None) };
}

fn last_prefs_snapshot_if_scope(scope: &str) -> Option<msg::SettingsPreferencesSnapshot> {
    LAST_PREFS_SNAPSHOT.with(|cell| {
        cell.borrow().as_ref().and_then(|(cached_scope, snap)| {
            (cached_scope == scope).then(|| snap.clone())
        })
    })
}

fn store_prefs_snapshot(snap: msg::SettingsPreferencesSnapshot) {
    LAST_PREFS_SNAPSHOT.with(|cell| {
        *cell.borrow_mut() = Some((snap.scope.clone(), snap));
    });
}

fn model_gateway_catalog_snapshot(model: &crate::model::Model) -> msg::SettingsGatewaySnapshot {
    msg::SettingsGatewaySnapshot {
        profiles: model.available_profiles.clone(),
        mcp_servers: model.available_mcp_servers.clone(),
        agents: model.agent_catalog.clone(),
        recoverable_sessions: model.recoverable_sessions.clone(),
        recovery_provider: model.recovery_provider.clone(),
    }
}

/// PISO-8 (project-isolation-mlt-binding plan): throttle for
/// `Effect::RefreshDaemonProjectInstances` -- same thread-local-timer
/// mechanism as `skills_rescan_due`, a longer 3s period since this poll
/// spawns a real subprocess (`snapshotd list`/`listProjects`) rather than
/// scanning a local directory.
fn daemon_projects_refresh_due() -> bool {
    thread_local! {
        static LAST_DAEMON_PROJECTS_POLL: std::cell::Cell<Option<std::time::Instant>> =
            const { std::cell::Cell::new(None) };
    }
    LAST_DAEMON_PROJECTS_POLL.with(|last| {
        let now = std::time::Instant::now();
        let due = last
            .get()
            .is_none_or(|at| now.duration_since(at) >= std::time::Duration::from_secs(3));
        if due {
            last.set(Some(now));
        }
        due
    })
}

pub(crate) struct ExternalSnapshotSource<'a> {
    panel: &'a PanelSingleton,
}

impl<'a> ExternalSnapshotSource<'a> {
    pub(crate) fn new(panel: &'a PanelSingleton) -> Self {
        Self { panel }
    }

    /// Copy completed ACP bindings into the TEA model before routing the
    /// events drained in this frame. This closes the attach window where
    /// `session/new`/`session/load` had completed in `AgentBridge`, but the
    /// model still had no remote session identity for map-based routing.
    fn hydrate_model_thread_bindings(&self) {
        let bindings = self
            .panel
            .bridge
            .as_ref()
            .map(|bridge| {
                (0..bridge.thread_count())
                    .filter_map(|index| bridge.thread_binding(index).map(|binding| (index, binding)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if bindings.is_empty() {
            return;
        }
        let mut model = self.panel.model.borrow_mut();
        let mut changed = false;
        for (index, binding) in bindings {
            let Some(thread) = model.threads.get_mut(index) else {
                continue;
            };
            if thread.thread_id != binding.thread_id {
                thread.thread_id = binding.thread_id;
                changed = true;
            }
            if thread.session_id.as_deref() != Some(binding.session_id.as_str()) {
                thread.session_id = Some(binding.session_id);
                changed = true;
            }
        }
        if changed {
            model.rebuild_thread_indices();
        }
    }

    pub(crate) fn collect_frame_input(&self) -> msg::FrameInput {
        let bridge_events = self
            .panel
            .bridge
            .as_ref()
            .map(AgentBridge::poll)
            .unwrap_or_default();
        self.hydrate_model_thread_bindings();
        let bridge_event_thread_ids = bridge_events
            .iter()
            .map(|event| {
                self.panel
                    .bridge
                    .as_ref()
                    .and_then(|bridge| bridge.thread_binding(event.thread_index))
                    .map(|binding| binding.thread_id)
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

        // Gateway catalog (profiles/agents/mcp) was previously only
        // collected while Settings is open. The compose-bar Provider
        // dropdown is driven by the same `available_profiles` list, so a
        // cold start that never opens Settings left profile_dropdown
        // empty (visible: length > 0) until the user happened to open
        // Settings once. Fetch when Settings is open *or* when the
        // catalogs are still empty so first-new-thread can show Provider.
        let (settings_open, need_gateway_catalog) = {
            let model = self.panel.model.borrow();
            (
                model.settings_open,
                model.settings_open
                    || model.available_profiles.is_empty()
                    || model.agent_catalog.is_empty(),
            )
        };
        let push_inventory_due = self
            .panel
            .snapshotd_registration
            .as_ref()
            .map(|registration| {
                #[cfg(unix)]
                {
                    registration.take_project_inventory_notification()
                }
                #[cfg(not(unix))]
                {
                    let _ = registration;
                    false
                }
            })
            .unwrap_or(false);
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
            settings_preferences_snapshot: (settings_open || settings_reload_pending)
                .then(|| self.collect_settings_preferences_snapshot(None)),
            // The bridge returns a cached catalog and starts any refresh on
            // its runtime. This path never waits for installation, RPC, or a
            // contended cache lock.
            settings_gateway_snapshot: need_gateway_catalog
                .then(|| self.collect_settings_gateway_snapshot()),
            agent_operations_in_flight: self
                .panel
                .bridge
                .as_ref()
                .map(AgentBridge::agent_operations_in_flight)
                .unwrap_or_default(),
            // Plan phase 27 (skills view reactivity): while Settings is on
            // screen, re-scan the skills dirs about once a second and fold
            // the result, so the live skills view tracks filesystem/state
            // changes (bundled getting-started appearing, dev-mode or
            // scope flips, external edits) instead of only refreshing
            // after this panel's own skill effects. Throttled because the
            // frame poll runs per repaint; the fold diffs by content, so
            // an unchanged scan dirties nothing.
            skills_snapshot: (settings_open && skills_rescan_due())
                .then(|| self.collect_skills_snapshot()),
            daemon_projects_refresh_due: daemon_projects_refresh_due() || push_inventory_due,
        }
    }

    /// Gateway catalog for Settings / compose provider dropdown.
    ///
    /// **UI-thread contract (lock_audit Layer 1):** never `block_on` RPC here.
    /// Kick a background refresh, then return the last cache fill only
    /// (same shape as `AgentBridge::poll` — background pushes, UI drains).
    pub(crate) fn collect_settings_gateway_snapshot(&self) -> msg::SettingsGatewaySnapshot {
        self.panel
            .bridge
            .as_ref()
            .map(|bridge| {
                let gw = self.panel.settings_gateway_index();
                // Push work off the frame-poll path (tokio), then drain cache.
                bridge.request_gateway_catalog_refresh(gw);
                let stale_fallback = model_gateway_catalog_snapshot(&self.panel.model.borrow());
                bridge.gateway_catalog_snapshot(stale_fallback)
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
        // Frame-poll path (up to 60-90x/sec): throttle disk/settings reads
        // (lock_audit F-04). Same pattern as skills_rescan_due — full JSON
        // load at most once per second while settings stay open.
        // Discard warnings here rather than Dirty::Error every tick —
        // cold-start call sites in lib.rs surface the same failures.
        if !prefs_rescan_due() {
            if let Some(cached) = last_prefs_snapshot_if_scope(scope) {
                return cached;
            }
            // Scope changed mid-window: fall through to one disk read.
        }
        let mut discarded_warnings = Vec::new();
        let prefs =
            crate::load_scoped_panel_prefs(scope, selected_thread_id.clone(), &mut discarded_warnings);
        let (defaults, default_agent_id) = prefs
            .map(|prefs| (prefs.defaults, prefs.default_agent_id))
            .unwrap_or_else(|| {
                let defaults =
                    crate::load_panel_prefs(selected_thread_id.clone(), &mut discarded_warnings);
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
                    .active_panel_state()
                    .as_ref()
                    .and_then(|store| store.thread_settings(thread_id).ok().flatten())
                    .and_then(|settings| settings.background_session)
                    .map(|value| (true, value))
            })
            .unwrap_or((false, false));
        let snap = msg::SettingsPreferencesSnapshot {
            scope: scope.to_owned(),
            default_profile: defaults.profile_name.unwrap_or_default(),
            permission_profile: defaults.permission_profile.unwrap_or_default(),
            background_default: defaults.background_session,
            default_agent_id: default_agent_id.unwrap_or_default(),
            dev_mode: crate::settings_file::SettingsPaths::from_env().dev_mode(),
            background_override_set,
            background_override,
        };
        store_prefs_snapshot(snap.clone());
        snap
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
                let active_store = self.panel.active_panel_state();
                let Some(store) = active_store.as_ref() else {
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
        let mut items = models::build_thread_items(
            &names,
            &state,
            &descriptions,
            &background_sessions,
            &closed,
            &archived,
            &query,
        );
        // Plan phase 26: the chat view binds to the selected project --
        // threads recorded against a DIFFERENT project drop out of the
        // visible list when the editor switches projects (path-less
        // threads stay). The phase-23 selection re-anchor keeps the
        // selected index sane across this rewrite.
        let active_project_path = self.panel.model.borrow().active_project_path.clone();
        models::retain_items_for_project(
            &mut items,
            &thread_project_paths,
            active_project_path.as_deref(),
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
                // PISO-5 (project-isolation-mlt-binding plan): the row's
                // project indicator shows ONLY this thread's own recorded
                // association -- see `models::display_project_path`'s doc
                // comment for why the former "inherit the active project
                // when unrecorded" fallback (phase 26/16) was retired
                // rather than kept now that PISO-3 supplies a real
                // recorded value for restored threads.
                let project_path = models::display_project_path(
                    thread_project_paths.get(item.real_index).map(String::as_str),
                );
                row.project_name = std::path::Path::new(&project_path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .into();
                // PISO-8 (project-isolation-mlt-binding plan): confirms
                // the badge above isn't just a stale sqlite-recorded
                // association -- true only when snapshotd's own
                // daemon.list/listProjects currently reports a live
                // ("ready") instance for this exact recorded project, per
                // `models::thread_project_instance_is_live`'s doc
                // comment.
                row.project_instance_live = models::thread_project_instance_is_live(
                    &project_path,
                    active_project_path.as_deref(),
                    &model_snapshot.live_daemon_projects,
                )
                .into();
                row.project_path = project_path.into();
                if let Some(thread) = model_snapshot.threads.get(item.real_index) {
                    row.profile_name = thread.profile_name.clone().unwrap_or_default().into();
                    row.has_session = thread.session_id.is_some();
                }
                // PUI-014: a DEFERRED slot (created but intentionally not yet
                // attached -- provider still editable, no message sent) is idle
                // and ready for input, NOT an attach-in-flight. Only show the
                // "Starting new thread..." loading state for a slot whose attach
                // has actually begun (eager/recovered) but not yet bound.
                if !row.closed
                    && self.panel.bridge.as_ref().is_some_and(|bridge| {
                        bridge.thread_binding(item.real_index).is_none()
                            && !bridge.is_deferred(item.real_index)
                    })
                {
                    row.status = "loading".into();
                    row.busy = true;
                    // Plan phase 30: immediate feedback while the
                    // background session attach is in flight -- the row
                    // appears instantly with a spinner (phase 25) and
                    // this caption instead of sitting silent.
                    if row.description.is_empty() {
                        row.description = "Starting new thread...".into();
                    }
                }
                let new_session_id = self
                    .panel
                    .bridge
                    .as_ref()
                    .and_then(|bridge| bridge.thread_binding(item.real_index))
                    .map(|binding| binding.session_id);
                // PROF-7: only probe agents/list at the exact frame a
                // session-attach transition (None -> Some) is about to be
                // folded -- matches update.rs's own `if
                // thread.session_id.is_none() { ... }` fold condition, so
                // this never runs on an already-attached thread's every
                // subsequent frame (agents/list is a real RPC).
                let already_attached = model_snapshot
                    .threads
                    .get(item.real_index)
                    .is_some_and(|thread| thread.session_id.is_some());
                // Use catalog cache only — never block_on agents/list on the
                // frame path (lock_audit F-01). Kick a background refresh if
                // cold; agent_detected may be None for one tick then settle.
                let agent_detected = if !already_attached && new_session_id.is_some() {
                    self.panel.bridge.as_ref().and_then(|bridge| {
                        bridge.request_gateway_catalog_refresh(item.real_index);
                        let catalog = bridge.gateway_catalog_snapshot(
                            model_gateway_catalog_snapshot(&model_snapshot),
                        );
                        models::agent_detected_for_profile(
                            &catalog.profiles,
                            &catalog.agents,
                            &row.profile_name,
                        )
                    })
                } else {
                    None
                };
                models::VisibleThreadItem {
                    real_index: item.real_index,
                    thread_id: thread_ids
                        .get(item.real_index)
                        .cloned()
                        .unwrap_or_else(|| format!("thread:{}", item.real_index)),
                    // Review-gate fix (phase 32): carry the live bridge
                    // binding so the frame fold can hydrate
                    // ThreadModel::session_id once a background attach
                    // resolves.
                    session_id: new_session_id,
                    agent_detected,
                    item: row,
                }
            })
            .collect();
        drop(model_snapshot);
        msg::ThreadListSnapshot {
            visible_indices,
            visible_thread_ids: rows.iter().map(|row| row.thread_id.clone()).collect(),
            rows,
            // Review-gate fix (phase 32): bridge-persisted archived flags
            // for every thread, so restarts hydrate ThreadModel::archived
            // (sidebar counters + archive pool cap read the model).
            archived_flags: archived,
            // PISO-2: tag with the SAME value `retain_items_for_project`
            // just filtered against above, not a fresh re-read -- the two
            // must never disagree about which project this snapshot's
            // shape reflects.
            active_project_path,
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
        let selected_provider = self
            .panel
            .model
            .borrow()
            .threads
            .get(real_idx)
            .and_then(|thread| thread.profile_name.as_deref())
            .and_then(|profile_name| {
                self.panel
                    .model
                    .borrow()
                    .available_profiles
                    .iter()
                    .find(|profile| profile.name == profile_name)
                    .map(|profile| profile.agent_id.clone())
            })
            .or_else(|| bridge.thread_provider(real_idx))
            .unwrap_or_default();
        bridge.ensure_models_for_provider(real_idx, &selected_provider);
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
        // Reuses `models::to_terminal_item_rows` (single-element vec)
        // rather than duplicating its field-population logic here --
        // this is the same TerminalItem shape the popup/chip's terminals
        // list uses (see `terminals` a few lines up), just narrowed to
        // one id (PUI-002c's expanded full-view source).
        let expanded_terminal = expanded_id.and_then(|id| {
            let buffer = bridge.terminal_buffer(real_idx, &id)?;
            models::to_terminal_item_rows(vec![(id, Some(buffer))]).into_iter().next()
        });
        // Terminal-tabs phase: resolve every currently-open tab id the same
        // way, in `open_terminal_ids`' own order (not `terminals`' order --
        // see `ThreadFrameSnapshot::open_terminals`'s doc comment). A stale
        // id whose buffer has since disappeared (e.g. `AgentBridge` dropped
        // it on thread teardown) still gets a row -- `to_terminal_item_rows`
        // renders a `None` buffer as an empty/inert placeholder item rather
        // than dropping it, same as the `terminals` list above -- so the
        // tab itself doesn't vanish out from under the user, it just goes
        // blank (closable the normal way).
        let open_ids = self.panel.model.borrow().open_terminal_ids.clone();
        let open_terminal_pairs = open_ids
            .into_iter()
            .map(|id| {
                let buffer = bridge.terminal_buffer(real_idx, &id);
                (id, buffer)
            })
            .collect();
        let open_terminals = models::to_terminal_item_rows(open_terminal_pairs);
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
            open_terminals,
            local_terminal: crate::models::to_local_terminal_item(
                bridge.local_terminal_snapshot(real_idx),
            ),
            connection_status: bridge.transport_status(real_idx),
            session_modes: bridge.session_modes(real_idx),
            usage: bridge.thread_usage(real_idx),
            config_options: bridge.config_options_for_provider(real_idx, &selected_provider),
            available_commands: bridge.available_commands(real_idx),
            plan: bridge.plan(real_idx),
            session_title: bridge.session_title(real_idx),
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
                    // PISO-3: persist the same project this thread's
                    // session was actually opened against (see
                    // `ThreadSlot::project_path`'s doc comment), not the
                    // currently-active project -- that distinction is the
                    // whole point of the durable association.
                    project_path: bridge.thread_project_path(idx),
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
            project_path: bridge.thread_project_path(real_idx),
        })
    }
}
