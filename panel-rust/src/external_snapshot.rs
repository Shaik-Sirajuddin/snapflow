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
        cell.borrow()
            .as_ref()
            .and_then(|(cached_scope, snap)| (cached_scope == scope).then(|| snap.clone()))
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
        agents_fetched: model.agent_catalog_fetched,
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

/// Copy completed ACP bindings into the TEA model before routing the events
/// drained in this frame. Two identities matter here, and they resolve at
/// different times:
///
/// - `AgentBridge::thread_binding` (full session binding: durable id +
///   remote session id) only becomes `Some` once `session/new`/`session/
///   load` has actually succeeded.
/// - `AgentBridge::thread_id` (the slot's durable pre-session identity, see
///   its own doc comment) is set the moment the slot is built -- before any
///   ACP request goes out -- specifically so a bridge event can be routed
///   by thread index before a session exists yet
///   (`collect_frame_input`'s `bridge_event_thread_ids` falls back to it
///   for exactly this).
///
/// A brand-new thread's model row is seeded with a synthetic
/// `"thread:{index}"` placeholder (`update.rs`'s `ThreadMsg::New`,
/// `visible_thread_row`'s doc comment: "fall back to synthetic so keys
/// always match ... when bridge binding was not yet known"). Previously
/// only a *successful* `thread_binding` ever replaced that placeholder
/// with the real durable id. A thread whose attach fails before ever
/// acquiring a session (e.g. autohand's "backend requires authentication
/// before session/new") never gets a `thread_binding` at all -- so the
/// model row's `thread_id` stayed `"thread:{index}"` forever, permanently
/// mismatched against the slug-based durable id the pre-session error's
/// `BridgeEvent` is routed under. `update_frame` then can't resolve
/// `model.thread_index_for_id(durable_id)`, silently drops the error
/// event as "stale or mid-attach" (its own comment: "the next frame will
/// retry once binding hydration makes the identity available" -- except
/// for a pre-session failure, hydration never ran, so no retry ever
/// surfaces it), and the thread is stuck in `Loading` with no way out.
///
/// Hydrating the durable pre-session id unconditionally (not just on
/// binding success) closes that gap: it collapses to the same value
/// `thread_binding` would have set anyway once a session exists (both
/// trace back to the same `ThreadSlot::thread_id`), so this changes
/// nothing for the already-working attach-succeeds path -- it only makes
/// the id available immediately instead of waiting on a session that, for
/// a pre-session failure, is never coming.
///
/// Split out of `ExternalSnapshotSource::hydrate_model_thread_bindings` as
/// a free function so this exact routing-identity invariant is
/// unit-testable against a real `AgentBridge` without needing a live
/// `PanelSingleton`/Slint platform (see `dispatch.rs`'s test module doc
/// comment for why that constructor requires headed Slint setup).
/// Returns whether anything changed, so the caller knows whether
/// `Model::rebuild_thread_indices` is needed.
pub(crate) fn hydrate_thread_ids_from_bridge(
    model: &mut crate::model::Model,
    bridge: &AgentBridge,
) -> bool {
    let mut changed = false;
    for index in 0..bridge.thread_count() {
        let Some(durable_id) = bridge.thread_id(index) else {
            continue;
        };
        let session_id = bridge
            .thread_binding(index)
            .map(|binding| binding.session_id);
        let Some(thread) = model.threads.get_mut(index) else {
            continue;
        };
        if thread.thread_id != durable_id {
            thread.thread_id = durable_id;
            changed = true;
        }
        if let Some(session_id) = session_id {
            if thread.session_id.as_deref() != Some(session_id.as_str()) {
                thread.session_id = Some(session_id);
                changed = true;
            }
        }
    }
    changed
}

/// PUI-014: a DEFERRED slot (created but intentionally not yet attached --
/// provider still editable, no message sent) is idle and ready for input,
/// NOT an attach-in-flight, so it never gets the "Starting new thread..."
/// loading affordance. A slot whose attach has actually begun (eager or
/// recovered) but has no binding yet *does* get it -- with one exception:
/// gateway-fail-state fix, `2026-08-01`. A binding never arrives after a
/// failed attach (gateway provisioning error, e.g. the real "Permission
/// denied (os error 13)" `spawn_gateway_process` failure -- see
/// `agent_bridge.rs`'s `ensure_executable`), so without the `status`
/// check this used to re-force "loading" on that exact row every single
/// frame forever, silently overwriting the correct "error" status
/// `models::build_thread_items` already computed from `ThreadState::Error`
/// (set by `update.rs`'s `AgentEvent::Error`/`SessionAttached{Err}`
/// handling, which both the async attach-failure path
/// (`spawn_background_attachment`) and the synchronous gateway-
/// provisioning-failure path (`dispatch_compose_send_maybe_attach`)
/// already route through) -- the thread looked permanently stuck loading
/// instead of showing its real failure. Once a frame has landed the row
/// on "error", this leaves it there; only a fresh attach attempt (which
/// re-derives `status` from `ThreadState` again, not this function) can
/// clear it.
fn shows_starting_thread_loading(
    closed: bool,
    status: &str,
    has_binding: bool,
    is_deferred: bool,
) -> bool {
    !closed && status != "error" && !has_binding && !is_deferred
}

pub(crate) struct ExternalSnapshotSource<'a> {
    panel: &'a PanelSingleton,
}

impl<'a> ExternalSnapshotSource<'a> {
    pub(crate) fn new(panel: &'a PanelSingleton) -> Self {
        Self { panel }
    }

    /// See [`hydrate_thread_ids_from_bridge`]'s doc comment for the full
    /// rationale; this method just supplies the live `PanelSingleton`'s
    /// bridge/model.
    fn hydrate_model_thread_bindings(&self) {
        let Some(bridge) = self.panel.bridge.as_ref() else {
            return;
        };
        let mut model = self.panel.model.borrow_mut();
        if hydrate_thread_ids_from_bridge(&mut model, bridge) {
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
                    .or_else(|| {
                        self.panel
                            .bridge
                            .as_ref()
                            .and_then(|bridge| bridge.thread_id(event.thread_index))
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
            mcp_operations_in_flight: self
                .panel
                .bridge
                .as_ref()
                .map(AgentBridge::mcp_operations_in_flight)
                .unwrap_or_default(),
            recover_session_operations_in_flight: self
                .panel
                .bridge
                .as_ref()
                .map(AgentBridge::recover_session_operations_in_flight)
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
                agents_fetched: false,
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
        let prefs = crate::load_scoped_panel_prefs(
            scope,
            selected_thread_id.clone(),
            &mut discarded_warnings,
        );
        let (defaults, default_agent_id, show_global_skills) = prefs
            .map(|prefs| {
                (
                    prefs.defaults,
                    prefs.default_agent_id,
                    prefs.show_global_skills,
                )
            })
            .unwrap_or_else(|| {
                let defaults =
                    crate::load_panel_prefs(selected_thread_id.clone(), &mut discarded_warnings);
                let resolved = crate::settings_file::SettingsPaths::from_env()
                    .load_resolved()
                    .ok();
                let default_agent_id = resolved.as_ref().and_then(|r| r.default_agent_id.clone());
                let show_global_skills = resolved.map(|r| r.show_global_skills).unwrap_or(true);
                (defaults, default_agent_id, show_global_skills)
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
            show_global_skills,
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
        // thread-unread-state: unlike archived/closed this lives on the TEA
        // ThreadModel, not on an AgentBridge slot -- it is in-memory only
        // (see ThreadModel::unread) and is folded by `update_frame`, so the
        // model is its single source of truth.
        let unread: Vec<bool> = model.threads.iter().map(|thread| thread.unread).collect();
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
                        models::describe_thread_from_last(
                            bridge.last_message(idx).as_ref(),
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
            &unread,
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
                    thread_project_paths
                        .get(item.real_index)
                        .map(String::as_str),
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
                // See `shows_starting_thread_loading`'s doc comment for the
                // full PUI-014 / gateway-fail-state contract this predicate
                // implements.
                if shows_starting_thread_loading(
                    row.closed,
                    row.status.as_str(),
                    self.panel
                        .bridge
                        .as_ref()
                        .is_some_and(|bridge| bridge.thread_binding(item.real_index).is_some()),
                    self.panel
                        .bridge
                        .as_ref()
                        .is_some_and(|bridge| bridge.is_deferred(item.real_index)),
                ) {
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
        let (selected_profile, model_thread_provider) = {
            let model = self.panel.model.borrow();
            let thread = model.threads.get(real_idx);
            (
                thread.and_then(|thread| thread.profile_name.clone()),
                thread
                    .map(|thread| thread.provider.clone())
                    .filter(|provider| !provider.is_empty()),
            )
        };
        // pre-send-config-options-visibility: `thread.provider` (the Model/
        // TEA layer) is what `SettingsMsg::ProfileSelected` actually keeps
        // current the instant the compose bar's Provider picker changes
        // (see update.rs's own "Critical: deferred attach uses thread.
        // provider... Leaving provider at create-time default made the
        // Provider picker a pure cosmetic change" comment) -- `AgentBridge::
        // thread_provider` (the fallback this used to end on) reads
        // `ThreadSlot::provider`, a plain field set once at thread
        // construction and never reassigned anywhere afterward. Live-
        // confirmed real bug this fixes: switching providers on a not-yet-
        // sent thread fetched the new provider's own capabilities
        // correctly (a real, successful models/list round trip) but the
        // dropdown stayed empty, because this resolution kept resolving
        // back to the thread's original (construction-time) provider via
        // the AgentBridge fallback -- so the fetch and the read population
        // it just also feeds were silently keyed on different providers.
        // The profile_name -> available_profiles lookup stays first: a
        // real, admin-provisioned profile is a more specific selection
        // than the plain provider id and should still win when present.
        let selected_provider = selected_profile
            .as_deref()
            .and_then(|profile_name| {
                self.panel
                    .model
                    .borrow()
                    .available_profiles
                    .iter()
                    .find(|profile| profile.name == profile_name)
                    .map(|profile| profile.agent_id.clone())
            })
            .or(model_thread_provider)
            .or_else(|| bridge.thread_provider(real_idx))
            .unwrap_or_default();
        bridge.ensure_models_for_provider(
            real_idx,
            &selected_provider,
            selected_profile.as_deref(),
        );
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
            models::to_terminal_item_rows(vec![(id, Some(buffer))])
                .into_iter()
                .next()
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
            config_options: bridge.config_options_for_provider(
                real_idx,
                &selected_provider,
                selected_profile.as_deref(),
            ),
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

    /// One `ThreadRecord` per bound thread that `update()`'s frame fold
    /// hasn't already persisted (`model.traced_attachment_threads`, checked
    /// via `HashSet::insert` in `update.rs`'s `for record in frame.
    /// thread_record_snapshots` loop -- every already-traced thread's
    /// record is built here just to be thrown away there). Filtering by the
    /// same set here, rather than after collection, means a poll tick
    /// (60-90fps) with N already-persisted open threads skips N thread_
    /// binding/thread_provider bridge lookups and ~7 `String` clones per
    /// thread (`display_name`, `profile_name`, `permission_profile`,
    /// `thread_id`, `session_id`, `project_path`, ...) that would otherwise
    /// be built and discarded on every single tick regardless of whether
    /// any new thread ever attaches.
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
                if model.traced_attachment_threads.contains(&binding.thread_id) {
                    return None;
                }
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

#[cfg(test)]
mod tests {
    //! `autohand-pre-session-error-hang` root-cause + fix coverage.
    //!
    //! The bug: a thread whose ACP session attach fails *before* a
    //! session ever forms (e.g. autohand's real, adapter-reported
    //! "backend requires authentication before session/new", triggered by
    //! the AutoHand CLI binary being uninstalled -- see
    //! `acpx/scripts/verify_autohand_acp.sh`) sat stuck in `Loading`
    //! forever: `AgentBridge` genuinely pushes a real
    //! `BridgeEvent::Error` (`agent_bridge.rs`'s `open_session`'s `Err`
    //! branch), but `update_frame` could never route it, because the
    //! model thread row's `thread_id` was still the synthetic
    //! `"thread:{index}"` placeholder `ThreadMsg::New` seeds every new
    //! thread with (`update.rs`) -- only a *successful*
    //! `AgentBridge::thread_binding` ever replaced it with the real
    //! durable id, and a pre-session failure never produces one.
    //!
    //! This drives the real, installed AutoHand ACP adapter through a
    //! real `acpx-server` (same mechanism `verify_autohand_acp.sh` uses)
    //! and exercises the exact production hydrate-then-route sequence
    //! `collect_frame_input`/`update_frame` run every frame, using real
    //! `AgentBridge`/`Model` methods -- just without a live
    //! `PanelSingleton`, which needs headed Slint setup this module's
    //! tests avoid (see `dispatch.rs`'s test module doc comment).
    use super::*;
    use crate::agent_bridge::{reserve_ephemeral_port, test_only_set_backend_cmd_env};
    use crate::model::{Model, ThreadModel};
    use crate::protocol_types::AgentEvent;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// gateway-fail-state (2026-08-01): pins the exact regression this fix
    /// closes -- a thread whose attach already failed (no binding, not a
    /// deliberately-unattached deferred slot -- indistinguishable from
    /// "attach still in flight" by binding/deferred state alone) must NOT
    /// have its already-correct "error" status forced back to "loading"
    /// on the next frame. Pure-function coverage (no `PanelSingleton`/
    /// `AgentBridge` needed) for the predicate `collect_thread_list_
    /// snapshot`'s per-row loop calls; see that call site's real
    /// production trigger (`spawn_gateway_process`'s "Permission denied
    /// (os error 13)" failure, `system_launch.yaml`) in this function's
    /// own doc comment.
    #[test]
    fn shows_starting_thread_loading_does_not_override_an_already_failed_attach() {
        // The exact state a thread lands in after `spawn_background_
        // attachment`'s error branch (or the synchronous gateway-
        // provisioning-failure path in `dispatch_compose_send_maybe_
        // attach`) has folded through to `ThreadState::Error` and
        // `models::build_thread_items` has already produced "error":
        // never bound, not deferred, not closed.
        assert!(
            !shows_starting_thread_loading(false, "error", false, false),
            "an already-failed attach's row must stay on its real \"error\" \
             status, not be forced back to \"loading\" forever"
        );
    }

    #[test]
    fn shows_starting_thread_loading_still_covers_the_real_in_flight_case() {
        // The case this predicate exists FOR: a genuinely in-flight eager/
        // recovered attach (status not yet "error" -- e.g. still "idle"
        // from the row's initial seed) with no binding yet and not
        // deferred. PUI-014's "Starting new thread..." affordance must
        // still show here; this fix must not have regressed it.
        assert!(shows_starting_thread_loading(false, "idle", false, false));
    }

    #[test]
    fn shows_starting_thread_loading_leaves_deferred_and_closed_rows_alone() {
        // PUI-014: a deferred (intentionally unattached) slot is idle and
        // ready for input, never shown as loading.
        assert!(!shows_starting_thread_loading(false, "idle", false, true));
        // A closed thread never gets the loading affordance either,
        // regardless of binding/status.
        assert!(!shows_starting_thread_loading(true, "idle", false, false));
        // A thread that already has a binding is attached; no loading
        // affordance needed even if some other status value slipped
        // through.
        assert!(!shows_starting_thread_loading(false, "idle", true, false));
    }

    fn autohand_adapter_entry() -> std::path::PathBuf {
        std::path::Path::new(&std::env::var("HOME").expect("HOME set"))
            .join(".acpx/adapters/autohand/node_modules/@autohandai/autohand-acp/dist/index.js")
    }

    fn acpx_server_bin() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../acpx/target/debug/acpx-server")
    }

    struct RealAutohandGateway {
        child: Child,
        base_url: String,
    }

    impl Drop for RealAutohandGateway {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    impl RealAutohandGateway {
        /// Real `acpx-server`, configured exactly like
        /// `verify_autohand_acp.sh`: `ACPX_DEFAULT_AGENT_ID=autohand` +
        /// the real installed adapter as the native default backend
        /// command (`ACPX_BACKEND_CMD` is `ACPX_DEFAULT_ACP_COMMAND`'s
        /// legacy-but-still-live alias, see `config.rs::from_env`'s doc
        /// comment) -- no mock anywhere in this path.
        fn spawn(adapter_entry: &std::path::Path) -> Self {
            let db_dir = std::env::temp_dir().join(format!(
                "panel-autohand-hang-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock")
                    .as_nanos()
            ));
            std::fs::create_dir_all(&db_dir).expect("create temp db dir");
            for attempt in 0..5 {
                let Some((port, lock)) = reserve_ephemeral_port() else {
                    std::thread::sleep(Duration::from_millis(50 * (attempt + 1)));
                    continue;
                };
                let mut command = Command::new(acpx_server_bin());
                command
                    .env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"))
                    .env("ACPX_DB_PATH", db_dir.join("acpx-test.db"))
                    .env("ACPX_DEFAULT_AGENT_ID", "autohand")
                    .env("RUST_LOG", "error")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                test_only_set_backend_cmd_env(
                    &mut command,
                    format!("node {}", adapter_entry.display()),
                );
                let child = command.spawn().expect("spawn real acpx-server binary");
                drop(lock);
                let base_url = format!("http://127.0.0.1:{port}");
                let gateway = RealAutohandGateway { child, base_url };
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if std::net::TcpStream::connect(format!("127.0.0.1:{port}")).is_ok() {
                        return gateway;
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                if attempt < 4 {
                    continue;
                }
                return gateway;
            }
            panic!("real acpx-server for autohand never became reachable");
        }
    }

    /// The exact routing logic `update_frame` (`update.rs`, ~line 2377)
    /// runs to resolve a `BridgeEvent`'s target thread: look up the
    /// event's durable/session identity in `model`'s thread-id index.
    /// Pulled out here (rather than depending on `update_frame` itself,
    /// which is private to `update.rs` and folds far more than routing)
    /// so this test proves precisely the invariant that broke: whether
    /// the model can resolve the identity `collect_frame_input` attaches
    /// to a given bridge event.
    fn resolve_target_index(model: &Model, durable_thread_id: &str) -> Option<usize> {
        model.thread_index_for_id(durable_thread_id)
    }

    /// Reproduces the pre-fix `hydrate_model_thread_bindings`: only ever
    /// syncs the model's `thread_id` from a *successful*
    /// `AgentBridge::thread_binding` (requires a live ACP session), never
    /// from the slot's durable pre-session `AgentBridge::thread_id`. Kept
    /// here, deliberately, as a literal stand-in for the code this test
    /// file's fix replaced, so the test can assert the *shipped* fix
    /// (`hydrate_thread_ids_from_bridge`, used above) actually resolves
    /// what this old logic could not -- not just that some behavior
    /// changed.
    fn hydrate_thread_ids_from_bridge_pre_fix(model: &mut Model, bridge: &AgentBridge) -> bool {
        let mut changed = false;
        for index in 0..bridge.thread_count() {
            let Some(binding) = bridge.thread_binding(index) else {
                continue;
            };
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
        changed
    }

    #[test]
    fn autohand_pre_session_failure_is_never_routed_by_the_pre_fix_hydration_but_is_by_the_real_fix(
    ) {
        let adapter_entry = autohand_adapter_entry();
        if !adapter_entry.exists() {
            eprintln!(
                "skipping: real AutoHand ACP adapter not installed at {} \
                 (see acpx/scripts/verify_autohand_acp.sh)",
                adapter_entry.display()
            );
            return;
        }
        if !acpx_server_bin().exists() {
            eprintln!(
                "skipping: acpx-server not built at {}, run \
                 `cargo build --manifest-path acpx/Cargo.toml -p acpx-server` first",
                acpx_server_bin().display()
            );
            return;
        }

        let gateway = RealAutohandGateway::spawn(&adapter_entry);
        let base_url = gateway.base_url.clone();
        let mut bridge = AgentBridge::new_with_gateway_resolver_and_cache_dir(
            &[],
            move |_provider| Ok(base_url.clone()),
            None,
        )
        .expect("bridge against the real autohand-backed acpx-server");

        // Mirrors `ThreadMsg::New` (`update.rs`) exactly: a brand-new
        // thread's model row is seeded with the synthetic
        // `"thread:{index}"` placeholder, *not* the bridge's own
        // slug-based durable id -- that mismatch is the root cause this
        // test exists to pin down.
        let real_index = 0usize;
        let mut model = Model {
            threads: vec![ThreadModel {
                thread_id: format!("thread:{real_index}"),
                display_name: "New thread 1".to_owned(),
                provider: "autohand".to_owned(),
                ..ThreadModel::default()
            }],
            ..Model::default()
        };
        model.rebuild_thread_indices();

        // PUI-014 deferred-create + first-send attach, the same shape
        // every real new thread goes through
        // (`dispatch_compose_send_maybe_attach`).
        let idx = bridge
            .add_thread_deferred("New thread 1", Some("autohand"))
            .expect("add_thread_deferred");
        assert_eq!(idx, real_index);
        bridge
            .attach_deferred_thread(idx, Some("autohand"), None)
            .expect("attach_deferred_thread");

        // Wait for the real adapter's real auth failure to actually
        // arrive as a `BridgeEvent::Error` -- proves `open_session`'s
        // `Err` branch really is hit for autohand, independent of any
        // routing question.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut error_event = None;
        while Instant::now() < deadline && error_event.is_none() {
            for event in bridge.poll() {
                if event.thread_index == idx {
                    if let AgentEvent::Error(message) = &event.event {
                        error_event = Some(message.clone());
                    }
                }
            }
            if error_event.is_none() {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let error_message = error_event.expect(
            "expected AgentBridge to push a real BridgeEvent::Error for autohand's pre-session \
             authentication failure within 20s",
        );
        assert!(
            error_message.to_lowercase().contains("authentic"),
            "expected the real autohand adapter's own authentication-required error, got: \
             {error_message:?}"
        );

        // The durable pre-session identity `collect_frame_input` routes
        // this event's thread_index through (`bridge_event_thread_ids`'s
        // `.or_else(bridge.thread_id(...))` fallback).
        let durable_thread_id = bridge
            .thread_id(idx)
            .expect("slot must have a durable thread_id once built");
        assert_ne!(
            durable_thread_id,
            format!("thread:{real_index}"),
            "the durable id must be the real slug, distinct from the synthetic placeholder \
             the model row starts with -- otherwise this test isn't exercising the mismatch"
        );

        // There was never a successful session, so there is no full
        // binding to hydrate from -- exactly the state that starved the
        // pre-fix hydration path.
        assert!(
            bridge.thread_binding(idx).is_none(),
            "autohand's pre-session failure must never produce a full session binding \
             (session/new never succeeded) -- if it did, this test would no longer be \
             covering the pre-session-failure gap"
        );

        // THE BUG, reproduced directly: the pre-fix hydration logic never
        // touches `model.threads[idx].thread_id` here (no binding to sync
        // from), so the model can never resolve the event's durable id --
        // `update_frame` would drop the error event as "stale or
        // mid-attach" forever, and the thread stays stuck in Loading.
        let mut pre_fix_model = model.clone();
        hydrate_thread_ids_from_bridge_pre_fix(&mut pre_fix_model, &bridge);
        assert_eq!(
            pre_fix_model.threads[idx].thread_id,
            format!("thread:{real_index}"),
            "pre-fix hydration must leave the synthetic placeholder in place -- this is the \
             exact stuck-in-Loading bug"
        );
        assert_eq!(
            resolve_target_index(&pre_fix_model, &durable_thread_id),
            None,
            "pre-fix hydration must be UNABLE to route the pre-session error event -- \
             reproduces the silent drop in update_frame that left autohand threads hanging"
        );

        // THE FIX: the real, shipped `hydrate_thread_ids_from_bridge`
        // syncs the durable pre-session id unconditionally, so the event
        // resolves.
        let changed = hydrate_thread_ids_from_bridge(&mut model, &bridge);
        assert!(
            changed,
            "the real fix must report a change once a durable id is available"
        );
        assert_eq!(
            model.threads[idx].thread_id, durable_thread_id,
            "the real fix must replace the synthetic placeholder with the bridge's durable id"
        );
        model.rebuild_thread_indices();
        assert_eq!(
            resolve_target_index(&model, &durable_thread_id),
            Some(idx),
            "after the real fix, the pre-session error event's durable id must resolve to the \
             thread that actually failed -- this is what lets update_frame surface the real \
             error instead of dropping it and leaving the thread stuck in Loading"
        );
    }
}
