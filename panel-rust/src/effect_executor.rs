//! Effect execution boundary for the TEA dispatcher.
//!
//! `update()` describes side effects; this module is the only production
//! code that executes those descriptions. Results re-enter through
//! `Msg::Effect`, while bridge/store snapshots re-enter through `Msg::Frame`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::dispatch::update_persistent;
use crate::effect::{Effect, EffectError, EffectResultMsg};
use crate::msg::{FrameInput, Msg};
use crate::PanelSingleton;
use slint::ComponentHandle;

/// Debounce window for edit-time reactive sync (README.md's "edit-time
/// reactive sync fires on every keystroke" gap) -- see
/// `schedule_debounced_skill_resync`'s own doc comment for the full
/// mechanism.
const SKILL_EDIT_DEBOUNCE: Duration = Duration::from_millis(750);

/// Bumped on every `Effect::SkillWrite` success; a debounced resync
/// attempt only proceeds if this is still the same value it captured
/// when scheduled, i.e. no newer edit has landed since. Global rather
/// than per-skill-path: only one skill editor is ever open at a time in
/// this UI, so a single counter is sufficient and avoids a HashMap plus
/// its own locking for no real benefit.
static SKILL_EDIT_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Generic debounce, extracted so the mechanism itself is unit-testable
/// without needing a real `PanelSingleton`: bumps `generation`, spawns a
/// thread that sleeps `delay`, then runs `on_settled` only if `generation`
/// is still the value this call bumped it to -- i.e. no newer `debounce`
/// call on the same counter landed during the sleep. `generation` is
/// `&'static` rather than owned so callers can share one counter across
/// many calls (production: `SKILL_EDIT_GENERATION`, one skill editor at a
/// time; tests: a fresh `Box::leak`'d counter per test, to avoid racing
/// with other tests sharing the same static).
///
/// Known, accepted TOCTOU window (self-review finding, not fixed --
/// negligible impact): a call can pass the `generation` check and then,
/// before `on_settled` actually starts, a *newer* call's `fetch_add` can
/// land, meaning at most one extra `on_settled` runs per burst instead of
/// the theoretical minimum of exactly one. Not a correctness bug for this
/// call site: `on_settled` (`schedule_debounced_skill_resync`) always
/// re-reads the skill's CURRENT on-disk content at execution time rather
/// than using anything captured when scheduled, so an extra run still
/// only ever propagates the latest content, never stale data -- it's
/// simply one redundant sync in the rare case this window is hit, not a
/// wrong result. Closing the window fully would need a mutex around
/// "check generation, then start on_settled" as one atomic step, which
/// isn't worth the added complexity for that benefit.
fn debounce(
    generation: &'static AtomicU64,
    delay: Duration,
    on_settled: impl FnOnce() + Send + 'static,
) {
    let my_generation = generation.fetch_add(1, Ordering::SeqCst) + 1;
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        if generation.load(Ordering::SeqCst) == my_generation {
            on_settled();
        }
        // Else: a newer call landed during the sleep -- that call's own
        // debounced attempt will run this same check later and (assuming
        // no further calls) proceed instead. This attempt is superseded,
        // not an error: do nothing, don't even log.
    });
}

/// Schedules reactive-sync trigger (3)'s edit half to run after
/// `SKILL_EDIT_DEBOUNCE` of no further edits, instead of on every single
/// keystroke `Effect::SkillWrite` fires for (Slint's `TextInput.edited`
/// has no debounce of its own -- confirmed by reading
/// `ui/pages/skills/skill_view.slint` directly). The actual
/// `enabled_vendor_ids` RPC call and all skills-manager work happen on a
/// background thread, and only after the debounce window elapses with no
/// newer edit superseding this one -- not on every call, and not
/// blocking whichever thread calls this function (re-enters the event
/// loop itself via `crate::PANEL` when it's actually time to run).
/// `skill_dir` is the skill's directory (same convention as everywhere
/// else in this file).
fn schedule_debounced_skill_resync(skill_dir: std::path::PathBuf) {
    // enabled_vendor_ids/project_root_from_skill_dir are computed on the
    // event-loop re-entry below (once per idle pause), not per keystroke.
    // enabled_vendor_ids is now a non-blocking read of the already-folded
    // model catalog (see its doc comment) -- it used to be a blocking
    // gateway RPC, which froze the UI thread here.
    debounce(&SKILL_EDIT_GENERATION, SKILL_EDIT_DEBOUNCE, move || {
        let _ = slint::invoke_from_event_loop(move || {
            crate::PANEL.with(|cell| {
                let slot = cell.borrow();
                let Some(panel) = slot.as_ref() else {
                    return;
                };
                // Non-blocking model read (see enabled_vendor_ids' doc
                // comment); runs once per idle pause, not per keystroke.
                let vendor_ids = enabled_vendor_ids(panel);
                let project_root =
                    crate::skills_manager_adapter::project_root_from_skill_dir(&skill_dir);
                std::thread::spawn(move || {
                    if let Err(error) =
                        crate::skills_manager_adapter::update_and_resync_edited_skill(
                            &skill_dir,
                            project_root.as_deref(),
                            &vendor_ids,
                        )
                    {
                        eprintln!(
                            "panel-rust: skills-manager edit-time reactive sync failed for {}: {error}",
                            skill_dir.display()
                        );
                        dispatch_reactive_sync_failed("edit", error.to_string());
                    }
                });
            });
        });
    });
}

/// Default frame poll does not collect skills. After filesystem skill
/// mutations, re-scan and fold an explicit skills snapshot so the list
/// stays in sync without a dual-path `refresh_skills_model`.
fn refresh_skills_after_effect(panel: &PanelSingleton) {
    let skills_snapshot =
        crate::external_snapshot::ExternalSnapshotSource::new(panel).collect_skills_snapshot();
    panel.dispatch_frame_input(FrameInput {
        skills_snapshot: Some(skills_snapshot),
        ..FrameInput::default()
    });
}

/// Reactive-sync trigger (1) (memory/acpx/gen/plans/acpx-skills/
/// README.md#reactive-sync): the "enabled-agent -> vendor_id map" turns
/// out to need no separate storage of its own -- `vendor_id` IS the ACP
/// registry agent id, and `AgentCatalogEntry::enabled` (already merged
/// client-side from the admin plane, see its own doc comment in
/// protocol_types.rs) already says which agent ids are currently enabled.
/// This is just that filter, reused everywhere trigger (3)/(4) need "which
/// vendor_ids should a skill mutation propagate to right now."
/// **Non-blocking.** Reads the agent catalog already folded into `Model`
/// (`update.rs`'s `model.agent_catalog = snapshot.agents`, refreshed from the
/// settings-gateway snapshot whenever settings are open or the catalog is
/// empty) rather than calling `AgentBridge::list_agents`, which is
/// `runtime.block_on`-backed (a gateway `agents/list` RPC, plus a second
/// admin-plane `block_on` in `agent_enablement_map`).
///
/// It previously did call `list_agents` directly, and every caller runs inside
/// `slint::invoke_from_event_loop` -- i.e. on the single-threaded Slint UI
/// thread -- so a skill edit/create/promote froze the whole UI for one or two
/// network round trips. That is the same bug class as `thread_new_loading_
/// state`/PUI-013 ("no blocking gateway call on the UI thread"): the fix is to
/// read state the frame poll already collected, exactly as `snapshotd_
/// reachable` serves the built-in-MCP row from a cached probe result instead of
/// re-probing on the UI thread.
///
/// Returns an empty list when the catalog has not been fetched yet, which the
/// reactive-sync callers already treat as "no enabled vendors to propagate to"
/// -- the same degrade-gracefully outcome the old RPC produced on failure.
fn enabled_vendor_ids(panel: &PanelSingleton) -> Vec<String> {
    panel
        .model
        .borrow()
        .agent_catalog
        .iter()
        .filter(|agent| agent.enabled)
        .map(|agent| agent.id.clone())
        .collect()
}

/// Surfaces a reactive-sync failure as a toast (memory/acpx/gen/plans/
/// acpx-skills/README.md's "reactive-sync failures are invisible to the
/// user" gap). Callable from any background thread -- re-enters the
/// event loop itself. `operation` is a short label (e.g. "create",
/// "edit") identifying which of the reactive-sync trigger call sites
/// failed; `detail` is the error's own message.
fn dispatch_reactive_sync_failed(operation: &'static str, detail: String) {
    let _ = slint::invoke_from_event_loop(move || {
        crate::PANEL.with(|cell| {
            let slot = cell.borrow();
            let Some(panel) = slot.as_ref() else {
                return;
            };
            let _ = update_persistent(
                panel,
                Msg::Effect(EffectResultMsg::SkillReactiveSyncFailed {
                    operation: operation.to_string(),
                    detail,
                }),
            );
        });
    });
}

/// Feeds one MCP server settings operation's outcome back through
/// `update_persistent` as `EffectResultMsg::McpServerOperationCompleted`,
/// arming the shared action-feedback toast -- same `slint::invoke_from_
/// event_loop` re-entry shape `Effect::SaveSettings`'s own handler above
/// already uses.
fn report_mcp_server_result(result: Result<String, String>) {
    let _ = slint::invoke_from_event_loop(move || {
        crate::PANEL.with(|cell| {
            let slot = cell.borrow();
            let Some(panel) = slot.as_ref() else {
                return;
            };
            let _ = update_persistent(
                panel,
                Msg::Effect(EffectResultMsg::McpServerOperationCompleted(
                    result.map_err(EffectError::new),
                )),
            );
        });
    });
}

fn execute_skill_effects(effects: Vec<Effect>) {
    for effect in effects {
        match effect {
            Effect::SkillWrite { path, content } => {
                std::thread::spawn(move || {
                    // `path` is the skill's DIRECTORY (same convention as
                    // every other skill effect -- OpenSkillEditor reads
                    // path.join("SKILL.md"), CopyPathRequested copies the
                    // directory, etc.), not the SKILL.md file itself.
                    // Pre-existing bug fixed here: this previously called
                    // std::fs::write(path, content) directly on that
                    // directory, which always fails with EISDIR -- the
                    // skill editor's save action had no working path to
                    // actually persist an edit, confirmed empirically
                    // (std::fs::write on a real directory reliably errors
                    // "Is a directory") while wiring reactive-sync trigger
                    // (3)'s edit half (memory/acpx/gen/plans/acpx-skills/).
                    let skill_md_path = path.join("SKILL.md");
                    let result = std::fs::write(&skill_md_path, &content)
                        .map_err(|error| EffectError::new(error.to_string()));
                    let write_succeeded = result.is_ok();
                    let _ = slint::invoke_from_event_loop(move || {
                        crate::PANEL.with(|cell| {
                            let slot = cell.borrow();
                            let Some(panel) = slot.as_ref() else {
                                return;
                            };
                            let _ = update_persistent(
                                panel,
                                Msg::Effect(EffectResultMsg::SkillWritten(result)),
                            );
                            // Content write does not change list identity; no skills rescan.

                            // Reactive-sync trigger (3)'s edit half.
                            // Debounced -- see schedule_debounced_skill_
                            // resync's own doc comment: Slint's
                            // TextInput.edited fires on every keystroke,
                            // so this must NOT run (or even compute
                            // enabled_vendor_ids's blocking RPC) on every
                            // call, only after edits actually settle.
                            // Only scheduled if the write actually
                            // succeeded -- syncing content that was never
                            // actually saved would be worse than not
                            // syncing at all.
                            if write_succeeded {
                                schedule_debounced_skill_resync(path.clone());
                            }
                        });
                    });
                });
            }
            Effect::CreateSkill {
                name,
                scope,
                active_project_path,
            } => {
                std::thread::spawn(move || {
                    let result = (|| {
                        let skill_scope = match scope.as_str() {
                            "global" => crate::skills_state::SkillScope::Global,
                            "project" => crate::skills_state::SkillScope::Project,
                            other => {
                                return Err(EffectError::new(format!(
                                    "invalid skill scope {other:?}"
                                )));
                            }
                        };
                        let active_project_file =
                            active_project_path.as_deref().map(std::path::Path::new);
                        let dir = crate::skills_state::skill_creation_dir(
                            skill_scope,
                            &crate::resolve_cache_dir(),
                            active_project_file,
                        )
                        .map_err(|error| EffectError::new(error.to_string()))?;
                        crate::skills_state::scaffold_new_skill(&dir, &name)
                            .map_err(|error| EffectError::new(error.to_string()))
                    })();
                    let _ = slint::invoke_from_event_loop(move || {
                        crate::PANEL.with(|cell| {
                            let slot = cell.borrow();
                            let Some(panel) = slot.as_ref() else {
                                return;
                            };
                            // Rescan *before* SkillCreated opens the
                            // editor: SkillCreated itself does not carry
                            // the new SkillEntry, and a post-open refresh
                            // was easy to miss if the follow-up effect
                            // short-circuited. Fold the fresh disk snapshot
                            // first so the skills list includes the new
                            // skill the moment the editor appears.
                            refresh_skills_after_effect(panel);

                            // Reactive-sync trigger (3)'s create half.
                            // Best-effort on its own background thread --
                            // a skill-sync hiccup must never block the
                            // editor from opening for the skill the user
                            // just created.
                            if let Ok(skill_path) = &result {
                                let skill_path = skill_path.clone();
                                let vendor_ids = enabled_vendor_ids(panel);
                                let project_root = (scope == "project")
                                    .then(|| active_project_path.as_deref())
                                    .flatten()
                                    .and_then(|file| std::path::Path::new(file).parent())
                                    .map(std::path::Path::to_path_buf);
                                std::thread::spawn(move || {
                                    if let Err(error) =
                                        crate::skills_manager_adapter::register_and_sync_new_skill(
                                            &skill_path,
                                            project_root.as_deref(),
                                            &vendor_ids,
                                        )
                                    {
                                        eprintln!(
                                            "panel-rust: skills-manager create-time reactive sync failed for {}: {error}",
                                            skill_path.display()
                                        );
                                        dispatch_reactive_sync_failed("create", error.to_string());
                                    }
                                });
                            }

                            let (follow_up, _) = update_persistent(
                                panel,
                                Msg::Effect(EffectResultMsg::SkillCreated(result)),
                            );
                            execute_effects(panel, follow_up);
                        });
                    });
                });
            }
            // skills_audit_report §3.2: disk read / process spawn must not
            // block the Slint UI thread.
            Effect::OpenSkillEditor { path } => {
                std::thread::spawn(move || {
                    let result = (|| {
                        let name = path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let md_path = path.join("SKILL.md");
                        let content = std::fs::read_to_string(&md_path)
                            .map_err(|error| EffectError::new(error.to_string()))?;
                        let detected_editors = crate::editor_detect::detect_installed_editors()
                            .into_iter()
                            .map(str::to_owned)
                            .collect();
                        Ok(crate::model::SkillEditorState {
                            name,
                            path: path.to_string_lossy().into_owned(),
                            content_path: md_path.to_string_lossy().into_owned(),
                            content,
                            detected_editors,
                        })
                    })();
                    let _ = slint::invoke_from_event_loop(move || {
                        crate::PANEL.with(|cell| {
                            let slot = cell.borrow();
                            let Some(panel) = slot.as_ref() else {
                                return;
                            };
                            let _ = update_persistent(
                                panel,
                                Msg::Effect(EffectResultMsg::SkillEditorLoaded(result)),
                            );
                        });
                    });
                });
            }
            Effect::OpenInEditor { editor_name, path } => {
                std::thread::spawn(move || {
                    let result = crate::editor_detect::EDITOR_CANDIDATES
                        .iter()
                        .find(|(_, name)| *name == editor_name)
                        .ok_or_else(|| EffectError::new(format!("unknown editor {editor_name:?}")))
                        .and_then(|(bin, _)| {
                            crate::editor_detect::open_in_editor(bin, std::path::Path::new(&path))
                                .map_err(|error| EffectError::new(error.to_string()))
                        });
                    let _ = slint::invoke_from_event_loop(move || {
                        crate::PANEL.with(|cell| {
                            let slot = cell.borrow();
                            let Some(panel) = slot.as_ref() else {
                                return;
                            };
                            let _ = update_persistent(
                                panel,
                                Msg::Effect(EffectResultMsg::ExternalEditorOpened(result)),
                            );
                        });
                    });
                });
            }
            Effect::OpenWithOsDefault { path } => {
                std::thread::spawn(move || {
                    let result =
                        crate::editor_detect::open_with_os_default(std::path::Path::new(&path))
                            .map_err(|error| EffectError::new(error.to_string()));
                    let _ = slint::invoke_from_event_loop(move || {
                        crate::PANEL.with(|cell| {
                            let slot = cell.borrow();
                            let Some(panel) = slot.as_ref() else {
                                return;
                            };
                            let _ = update_persistent(
                                panel,
                                Msg::Effect(EffectResultMsg::OsDefaultOpened(result)),
                            );
                        });
                    });
                });
            }
            Effect::ClipboardWrite { text } => {
                std::thread::spawn(move || {
                    // Best-effort system clipboard without a new crate dep:
                    // wl-copy (Wayland) then xclip (X11).
                    let _ = write_clipboard_text(&text);
                });
            }
            Effect::SkillPromoteToGlobal { path } => {
                std::thread::spawn(move || {
                    let cache_dir = crate::resolve_cache_dir();
                    let global_dir = crate::skills_state::global_skills_dir(&cache_dir);
                    let promoted_path =
                        crate::skills_state::promote_skill_to_global(&path, &global_dir);
                    let new_skill_path = promoted_path.as_ref().ok().cloned();
                    let result = promoted_path
                        .map(|_| ())
                        .map_err(|error| EffectError::new(error.to_string()));
                    let _ = slint::invoke_from_event_loop(move || {
                        crate::PANEL.with(|cell| {
                            let slot = cell.borrow();
                            let Some(panel) = slot.as_ref() else {
                                return;
                            };
                            let _ = update_persistent(
                                panel,
                                Msg::Effect(EffectResultMsg::SkillPromoted(result)),
                            );
                            refresh_skills_after_effect(panel);

                            // Reactive-sync trigger (3)'s create half
                            // applies to promotion too -- a promoted
                            // skill is now global content that should
                            // propagate to every enabled agent, same as
                            // a freshly created one. Global scope
                            // (project_root: None), since promotion's
                            // whole point is leaving project-local
                            // storage.
                            if let Some(new_skill_path) = new_skill_path {
                                let vendor_ids = enabled_vendor_ids(panel);
                                std::thread::spawn(move || {
                                    if let Err(error) =
                                        crate::skills_manager_adapter::register_and_sync_new_skill(
                                            &new_skill_path,
                                            None,
                                            &vendor_ids,
                                        )
                                    {
                                        eprintln!(
                                            "panel-rust: skills-manager promote-to-global reactive sync failed for {}: {error}",
                                            new_skill_path.display()
                                        );
                                        dispatch_reactive_sync_failed("promote", error.to_string());
                                    }
                                });
                            }
                        });
                    });
                });
            }
            other => {
                debug_assert!(
                    false,
                    "skill effect executor received non-skill effect: {other:?}"
                );
            }
        }
    }
}

/// Execute bridge-, store-, and filesystem-backed effects emitted by
/// `update()`. Effects are deliberately kept out of the reducer and out of
/// the callback wrappers.
pub(crate) fn execute_effects(panel: &PanelSingleton, effects: Vec<Effect>) {
    if effects.is_empty() {
        return;
    }
    let mut refresh_frame = false;
    for effect in effects {
        refresh_frame |= !matches!(
            effect,
            Effect::LoadInitialState | Effect::PersistSelectedThread { .. }
        );
        match effect {
            Effect::LoadInitialState => {}
            Effect::SendPrompt { thread_id, text } => {
                let real_index = panel.model.borrow().thread_index_for_id(&thread_id);
                if let Some(real_index) = real_index {
                    panel.execute_send_prompt_real(real_index, &text);
                }
            }
            Effect::CancelGeneration { real_index } => {
                panel.execute_cancel_generation_real(real_index);
            }
            Effect::RespondAgentRequest { approve, .. } => {
                panel.answer_pending_request(approve);
            }
            Effect::PermissionOptionSelected { option, .. } => {
                panel.answer_pending_request_option(&option);
            }
            Effect::LoadOlderMessages { .. } => {
                panel.dispatch_load_older_requested();
            }
            Effect::LocalTerminalSpawn => {
                panel.dispatch_local_terminal_toggle();
            }
            Effect::LocalTerminalKill => {
                panel.dispatch_local_terminal_close();
            }
            Effect::LocalTerminalWrite { bytes } => {
                let text = String::from_utf8_lossy(&bytes);
                panel.dispatch_local_terminal_key_input(&text);
            }
            Effect::KillAgentTerminal {
                real_index,
                terminal_id,
            } => {
                panel.execute_kill_agent_terminal_real(real_index, terminal_id);
            }
            Effect::SaveSettings { input } => {
                let result = panel.execute_settings_save(input);
                let _ = slint::invoke_from_event_loop(move || {
                    crate::PANEL.with(|cell| {
                        let slot = cell.borrow();
                        let Some(panel) = slot.as_ref() else {
                            return;
                        };
                        let _ = update_persistent(
                            panel,
                            Msg::Effect(EffectResultMsg::SettingsSaved(result)),
                        );
                    });
                });
            }
            Effect::SetConfigOption { key, value, .. } => {
                panel.dispatch_config_option_selected(&key, &value);
            }
            Effect::SetMode { mode, .. } => {
                panel.dispatch_mode_selected(&mode);
            }
            Effect::SaveDevMode { enabled } => {
                panel.dispatch_dev_mode_toggled(enabled);
            }
            Effect::McpServerCreate { entry, .. } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                let result = panel.dispatch_mcp_server_create(&component, entry);
                report_mcp_server_result(result);
            }
            Effect::McpServerUpdate { entry, .. } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                let result = panel.dispatch_mcp_server_update(&component, entry);
                report_mcp_server_result(result);
            }
            Effect::McpServerDelete { name, .. } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                let result = panel.dispatch_mcp_server_delete(&component, &name);
                report_mcp_server_result(result);
            }
            Effect::McpServerEnabledChanged { name, enabled, .. } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                let result = panel.dispatch_mcp_server_enabled_changed(&component, &name, enabled);
                report_mcp_server_result(result);
            }
            Effect::McpServerAuthenticate { name, .. } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                let result = panel.dispatch_mcp_server_authenticate(&component, &name);
                report_mcp_server_result(result);
            }
            Effect::McpServerLogout { name, .. } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                let result = panel.dispatch_mcp_server_logout(&component, &name);
                report_mcp_server_result(result);
            }
            Effect::McpServerToolEnabledChanged {
                server_name,
                tool_name,
                enabled,
                ..
            } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                let result = panel.dispatch_mcp_server_tool_enabled_changed(
                    &component,
                    &server_name,
                    &tool_name,
                    enabled,
                );
                report_mcp_server_result(result);
            }
            Effect::McpServerToolDeferredChanged {
                server_name,
                tool_name,
                deferred,
                ..
            } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                let result = panel.dispatch_mcp_server_tool_deferred_changed(
                    &component,
                    &server_name,
                    &tool_name,
                    deferred,
                );
                report_mcp_server_result(result);
            }
            Effect::McpServerToolsFetchRequested { server_name, .. } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                let result = panel.dispatch_mcp_server_tools_fetch(&component, &server_name);
                report_mcp_server_result(result);
            }
            Effect::ProfileCreate {
                name,
                agent_id,
                terminal_enabled,
                fs_enabled,
                ..
            } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                panel.dispatch_profile_create(
                    &component,
                    &name,
                    agent_id.as_deref(),
                    terminal_enabled,
                    fs_enabled,
                );
            }
            Effect::ProfileDelete { name, .. } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                panel.dispatch_profile_delete(&component, &name);
            }
            Effect::AgentInstallRequested { agent_id, .. } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                panel.dispatch_agent_install_requested(&component, &agent_id);
            }
            Effect::AgentSetEnabled {
                agent_id, enabled, ..
            } => {
                panel.dispatch_agent_set_enabled(&agent_id, enabled);
                // Reactive-sync trigger (4) (memory/acpx/gen/plans/
                // acpx-skills/README.md#reactive-sync): backfill on
                // enable, explicit teardown (not just future-sync
                // suppression) on disable. Global scope only here --
                // agent enablement is gateway-wide, not tied to any one
                // open project (same posture AgentBridge::set_agent_enabled's
                // own doc comment states); project-scoped skills for a
                // given open project get reconciled at thread-start
                // (trigger (2)) instead. Best-effort on its own
                // background thread, same as execute_skill_effects'
                // filesystem effects -- a skill-sync hiccup must never
                // block or fail the agent enable/disable action itself.
                std::thread::spawn(move || {
                    let result = if enabled {
                        crate::skills_manager_adapter::sync_agent_targets(&agent_id, None)
                            .map(|_| ())
                    } else {
                        crate::skills_manager_adapter::teardown_agent_targets(&agent_id, None)
                    };
                    if let Err(error) = result {
                        eprintln!(
                            "panel-rust: skills-manager agent-enabled reactive sync failed for {agent_id} (enabled={enabled}): {error}"
                        );
                        let operation = if enabled {
                            "agent-enable"
                        } else {
                            "agent-disable"
                        };
                        dispatch_reactive_sync_failed(operation, error.to_string());
                    }
                });
            }
            Effect::SkillWrite { .. }
            | Effect::CreateSkill { .. }
            | Effect::SkillPromoteToGlobal { .. }
            | Effect::OpenSkillEditor { .. }
            | Effect::OpenInEditor { .. }
            | Effect::OpenWithOsDefault { .. }
            | Effect::ClipboardWrite { .. } => {
                execute_skill_effects(vec![effect]);
            }
            Effect::SetActiveProjectPath { path } => {
                panel.apply_active_project_path(path);
            }
            Effect::RefreshDaemonProjectInstances => {
                // Real subprocess spawns + (inside the CLI) a real Unix
                // socket dial -- off the UI thread, per `agent_bridge::
                // fetch_daemon_project_instances`'s own doc comment.
                std::thread::spawn(move || {
                    let result = crate::agent_bridge::fetch_daemon_project_instances()
                        .map_err(EffectError::new);
                    let _ = slint::invoke_from_event_loop(move || {
                        crate::PANEL.with(|cell| {
                            let slot = cell.borrow();
                            let Some(panel) = slot.as_ref() else {
                                return;
                            };
                            let _ = update_persistent(
                                panel,
                                Msg::Effect(EffectResultMsg::DaemonProjectInstancesLoaded(result)),
                            );
                        });
                    });
                });
            }
            Effect::RenameProjectAssociation {
                old,
                new,
                old_identity,
            } => {
                // Rewrite the live SQLite rows before moving its directory.
                // An open SQLite connection can become read-only when its
                // containing directory is renamed first (especially with a
                // WAL/journal present), which would leave Save-As looking
                // correct in memory but stale after restart.
                // `update_host(ProjectPathRenamed)` has already folded the
                // destination identity into the model. Resolve the source
                // store by its durable old path; using active_panel_state()
                // here would create the destination store prematurely and
                // make the filesystem move a no-op.
                let store = panel.project_state_for_identity(&old_identity);
                if let Some(store) = store.as_ref() {
                    let result = if old.is_empty() {
                        store.assign_unscoped_project_path(&new)
                    } else {
                        store.rename_project_path(&old, &new)
                    };
                    if let Err(error) = result {
                        eprintln!(
                            "panel-rust: failed to persist project rename {old:?} -> {new:?}: {error}"
                        );
                    }
                }
                panel.move_project_store_for_rename(&old_identity, &new);
                // Synchronous, in-memory only (no I/O) -- must run before
                // this call returns, not spawned, so the very next poll
                // tick already sees the rebind and the running session's
                // sidebar self-heals without waiting on the sqlite write
                // below. See `AgentBridge::rebind_project_path`'s doc
                // comment for why sqlite alone isn't enough on its own.
                if let Some(bridge) = panel.bridge.as_ref() {
                    if old.is_empty() {
                        bridge.rebind_unscoped_project_path(&new);
                    } else {
                        bridge.rebind_project_path(&old, &new);
                    }
                }
                // Durable half: survives a restart. This write is kept
                // synchronous at the lifecycle boundary so Save-As cannot
                // return with the in-memory association moved while SQLite
                // still contains the old project path.
            }
            Effect::CloseThread { real_index } => {
                if let Some(bridge) = panel.bridge.as_ref() {
                    // The actual wiring for this thread's own "background"
                    // toggle (previously stored and displayed, but never
                    // connected to any real close-session behavior) --
                    // see AgentBridge::close_thread's doc comment.
                    let thread_id = bridge
                        .thread_binding(real_index)
                        .map(|binding| binding.thread_id);
                    let store = panel.active_panel_state();
                    let background = thread_id
                        .as_ref()
                        .and_then(|thread_id| {
                            store.as_ref().and_then(|store| {
                                store.effective_background_session(thread_id).ok()
                            })
                        })
                        .unwrap_or(false);
                    if !bridge.close_thread(real_index, background) {
                        let message = format!("failed to close thread {real_index}");
                        eprintln!("panel-rust: {message}");
                        let _ = update_persistent(
                            panel,
                            Msg::Effect(EffectResultMsg::StateEffectFailed {
                                thread_id: thread_id.unwrap_or_default(),
                                message,
                            }),
                        );
                    }
                }
            }
            Effect::ArchiveThread {
                real_index,
                archived,
            } => {
                if let Some(bridge) = panel.bridge.as_ref() {
                    // Keep main's toggle-capable archive (archived=false
                    // resumes) and adopt the audit branch's failure surfacing
                    // into Dirty::Error via StateEffectFailed.
                    let thread_id = bridge
                        .thread_binding(real_index)
                        .map(|binding| binding.thread_id)
                        .unwrap_or_default();
                    if !bridge.set_thread_archived(real_index, archived) {
                        let message =
                            format!("failed to set thread {real_index} archived={archived}");
                        eprintln!("panel-rust: {message}");
                        let _ = update_persistent(
                            panel,
                            Msg::Effect(EffectResultMsg::StateEffectFailed { thread_id, message }),
                        );
                    }
                }
            }
            Effect::PersistSelectedThread { thread_id } => {
                let Some(store) = panel.active_panel_state() else {
                    continue;
                };
                std::thread::spawn(move || {
                    if let Err(error) = store.set_selected_thread_id(Some(&thread_id)) {
                        let message = format!("failed to persist selected chat thread: {error}");
                        eprintln!("panel-rust: {message}");
                        let _ = slint::invoke_from_event_loop(move || {
                            crate::PANEL.with(|cell| {
                                let slot = cell.borrow();
                                let Some(panel) = slot.as_ref() else {
                                    return;
                                };
                                let _ = update_persistent(
                                    panel,
                                    Msg::Effect(EffectResultMsg::StateEffectFailed {
                                        thread_id,
                                        message,
                                    }),
                                );
                            });
                        });
                    }
                });
            }
            Effect::ToggleBackground { real_index } => {
                let Some(store) = panel.active_panel_state() else {
                    continue;
                };
                let Some(thread_id) = panel
                    .bridge
                    .as_ref()
                    .and_then(|bridge| bridge.thread_binding(real_index))
                    .map(|binding| binding.thread_id)
                else {
                    continue;
                };
                std::thread::spawn(move || {
                    let next = !store
                        .effective_background_session(&thread_id)
                        .unwrap_or(false);
                    if let Err(error) = store.set_background_override(&thread_id, Some(next)) {
                        let message =
                            format!("failed to toggle background-session override: {error}");
                        eprintln!("panel-rust: {message}");
                        let _ = slint::invoke_from_event_loop(move || {
                            crate::PANEL.with(|cell| {
                                let slot = cell.borrow();
                                let Some(panel) = slot.as_ref() else {
                                    return;
                                };
                                let _ = update_persistent(
                                    panel,
                                    Msg::Effect(EffectResultMsg::StateEffectFailed {
                                        thread_id,
                                        message,
                                    }),
                                );
                            });
                        });
                    } else {
                        let _ = slint::invoke_from_event_loop(move || {
                            crate::PANEL.with(|cell| {
                                let slot = cell.borrow();
                                let Some(panel) = slot.as_ref() else {
                                    return;
                                };
                                if let Some(bridge) = panel.bridge.as_ref() {
                                    bridge.set_thread_background(real_index, next);
                                }
                            });
                        });
                    }
                });
            }
            Effect::PersistThreadRecord { record } => {
                let store = panel.active_panel_state();
                std::thread::spawn(move || {
                    let result = store
                        .map(|store| {
                            store
                                .save_thread_record(&record)
                                .map_err(|error| EffectError::new(error.to_string()))
                        })
                        .unwrap_or(Ok(()));
                    let _ = slint::invoke_from_event_loop(move || {
                        crate::PANEL.with(|cell| {
                            let slot = cell.borrow();
                            let Some(panel) = slot.as_ref() else {
                                return;
                            };
                            let _ = update_persistent(
                                panel,
                                Msg::Effect(EffectResultMsg::ThreadRecordPersisted(result)),
                            );
                        });
                    });
                });
            }
            Effect::PersistThread { real_index } => {
                // collect_thread_record reads live model/bridge state, so it
                // must run on this (UI) thread; only the blocking SQLite
                // write itself is offloaded.
                let record = crate::external_snapshot::ExternalSnapshotSource::new(panel)
                    .collect_thread_record(real_index);
                let store = panel.active_panel_state();
                std::thread::spawn(move || {
                    let result = record.map(|record| {
                        store
                            .map(|store| {
                                store
                                    .save_thread_record(&record)
                                    .map_err(|error| EffectError::new(error.to_string()))
                            })
                            .unwrap_or(Ok(()))
                    });
                    let result = result.unwrap_or(Ok(()));
                    let _ = slint::invoke_from_event_loop(move || {
                        crate::PANEL.with(|cell| {
                            let slot = cell.borrow();
                            let Some(panel) = slot.as_ref() else {
                                return;
                            };
                            let _ = update_persistent(
                                panel,
                                Msg::Effect(EffectResultMsg::ThreadPersisted {
                                    real_index,
                                    result,
                                }),
                            );
                        });
                    });
                });
            }
            Effect::RenameThread { real_index, name } => {
                // Thread-id lookup needs the (non-Send) model RefCell, so it
                // must happen here; only the blocking SQLite write moves to
                // the spawned thread.
                let thread_id = panel
                    .model
                    .borrow()
                    .threads
                    .get(real_index)
                    .map(|thread| thread.thread_id.clone());
                let store = panel.active_panel_state();
                if let (Some(store), Some(thread_id)) = (store, thread_id) {
                    std::thread::spawn(move || {
                        if let Err(error) = store.update_thread_display_name(&thread_id, &name) {
                            let message = format!("failed to persist renamed chat thread: {error}");
                            eprintln!("panel-rust: {message}");
                            let _ = slint::invoke_from_event_loop(move || {
                                crate::PANEL.with(|cell| {
                                    let slot = cell.borrow();
                                    let Some(panel) = slot.as_ref() else {
                                        return;
                                    };
                                    let _ = update_persistent(
                                        panel,
                                        Msg::Effect(EffectResultMsg::StateEffectFailed {
                                            thread_id,
                                            message,
                                        }),
                                    );
                                });
                            });
                        }
                    });
                }
            }
            Effect::DeleteThread { real_index } => {
                if let Some(bridge) = panel.bridge.as_ref() {
                    let thread_id = bridge
                        .thread_binding(real_index)
                        .map(|binding| binding.thread_id)
                        .unwrap_or_default();
                    if !bridge.delete_thread(real_index) {
                        let message = format!("failed to delete thread {real_index}");
                        eprintln!("panel-rust: {message}");
                        let _ = update_persistent(
                            panel,
                            Msg::Effect(EffectResultMsg::StateEffectFailed { thread_id, message }),
                        );
                    }
                }
            }
            Effect::SkillDelete { path } => {
                std::thread::spawn(move || {
                    let result = std::fs::remove_dir_all(&path)
                        .map_err(|error| EffectError::new(error.to_string()));
                    if let Err(error) = &result {
                        eprintln!("panel-rust: failed to delete skill: {error}");
                    }
                    let _ = slint::invoke_from_event_loop(move || {
                        crate::PANEL.with(|cell| {
                            let slot = cell.borrow();
                            let Some(panel) = slot.as_ref() else {
                                return;
                            };
                            let _ = update_persistent(
                                panel,
                                Msg::Effect(match result {
                                    Ok(()) => EffectResultMsg::SkillWritten(Ok(())),
                                    Err(error) => EffectResultMsg::SkillWritten(Err(error)),
                                }),
                            );
                            refresh_skills_after_effect(panel);
                        });
                    });
                });
            }
            Effect::NewThreadDeferred { .. } | Effect::RecoverSessionAttach { .. } => {
                debug_assert!(
                    false,
                    "thread lifecycle effects must use execute_thread_lifecycle_effect"
                );
            }
        }
    }
    // Effects may change bridge/store state without producing a typed
    // completion payload. Re-enter through the external Frame source so
    // update()/sync() fold and project those changes in one place.
    if refresh_frame {
        crate::dispatch::dispatch_frame_poll(panel);
    }
}

#[cfg(test)]
mod skill_editor_path_tests {
    // PUI-010: reproduces the real OS error this bug produced, and proves
    // the fix -- `SkillEditorState.path` (the skill directory) must never
    // be the target of a content write; `content_path` (SKILL.md inside
    // it) must be. No Slint event loop needed here: this is exactly the
    // filesystem mechanics `Effect::OpenSkillEditor`'s closure and
    // `Effect::SkillWrite`'s handler perform, exercised directly against
    // a real temp directory standing in for a real skill folder.
    use std::io::ErrorKind;

    #[test]
    fn writing_to_the_skill_directory_itself_hits_eisdir() {
        // This is the bug: before content_path existed, ContentEdited's
        // path (== SkillEditorState.path == the directory) flowed
        // straight into Effect::SkillWrite's `std::fs::write(path, ..)`.
        let dir = tempfile_dir();
        let err = std::fs::write(&dir, "content").unwrap_err();
        assert_eq!(err.kind(), ErrorKind::IsADirectory);
    }

    #[test]
    fn open_skill_editor_derives_a_writable_content_path_distinct_from_the_directory() {
        let dir = tempfile_dir();
        let md_path = dir.join("SKILL.md");
        std::fs::write(&md_path, "---\nname: demo\n---\nold body").unwrap();

        // Mirrors Effect::OpenSkillEditor's closure exactly (minus the
        // thread hop / Slint event-loop re-entry, which need a live
        // ChatPanel and are covered by the reducer tests in update.rs).
        let content = std::fs::read_to_string(&md_path).unwrap();
        let state = crate::model::SkillEditorState {
            name: dir.file_name().unwrap().to_string_lossy().into_owned(),
            path: dir.to_string_lossy().into_owned(),
            content_path: md_path.to_string_lossy().into_owned(),
            content,
            detected_editors: vec![],
        };

        assert_ne!(state.path, state.content_path);
        assert!(std::path::Path::new(&state.content_path).is_file());

        // The fix in one line: writing to content_path (not path) succeeds.
        std::fs::write(&state.content_path, "old body plus a typed delta").unwrap();
        assert_eq!(
            std::fs::read_to_string(&state.content_path).unwrap(),
            "old body plus a typed delta"
        );
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "panel-rust-skill-eisdir-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

/// PUI select-and-copy: also the delegate target for `SpikePlatform::
/// set_clipboard_text` (`lib.rs`) -- Slint's built-in Ctrl+C inside a
/// `TextInput` calls `Platform::set_clipboard_text` directly (see
/// `i-slint-core`'s `text.rs::copy_clipboard`), and this custom software-
/// rendering platform has no OS clipboard integration of its own, so
/// without that delegation a real selection's Ctrl+C was a silent no-op.
/// `pub(crate)` (not `pub`) since only this crate's own platform impl and
/// `Effect::ClipboardWrite` above call it.
pub(crate) fn write_clipboard_text(text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    for (bin, args) in [
        ("wl-copy", Vec::<&str>::new()),
        ("xclip", vec!["-selection", "clipboard"]),
        ("xsel", vec!["--clipboard", "--input"]),
    ] {
        let Ok(mut child) = Command::new(bin)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        if child.wait().map(|s| s.success()).unwrap_or(false) {
            return Ok(());
        }
    }
    Err("no clipboard helper (wl-copy/xclip/xsel) available".into())
}

/// Read the system clipboard for Slint's native TextInput paste path.
/// Keep the helper order aligned with `write_clipboard_text` so Wayland and
/// X11 sessions use the same small external-tool fallback strategy.
pub(crate) fn read_clipboard_text() -> Option<String> {
    use std::process::{Command, Stdio};

    let helpers: [(&str, &[&str]); 3] = [
        ("wl-paste", &["--no-newline"]),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
    ];
    for (bin, args) in helpers {
        let Ok(output) = Command::new(bin)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        else {
            continue;
        };
        if output.status.success() {
            return String::from_utf8(output.stdout).ok();
        }
    }
    None
}

#[cfg(test)]
mod debounce_tests {
    use super::debounce;
    use std::sync::atomic::AtomicU64;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// A fresh, isolated counter per test -- `debounce` takes `&'static
    /// AtomicU64` so calls can share one counter across many invocations
    /// (production: one counter for the whole app, since only one skill
    /// editor is ever open at a time); `Box::leak` mints a real `'static`
    /// reference scoped to just this test, so parallel test runs don't
    /// race on a single shared static.
    fn fresh_counter() -> &'static AtomicU64 {
        Box::leak(Box::new(AtomicU64::new(0)))
    }

    #[test]
    fn a_single_call_settles_and_runs_after_the_delay() {
        let counter = fresh_counter();
        let ran = Arc::new(Mutex::new(false));
        let ran_clone = ran.clone();
        debounce(counter, Duration::from_millis(30), move || {
            *ran_clone.lock().unwrap() = true;
        });

        // Must not have run yet -- still inside the debounce window.
        std::thread::sleep(Duration::from_millis(10));
        assert!(
            !*ran.lock().unwrap(),
            "must not run before the delay elapses"
        );

        std::thread::sleep(Duration::from_millis(40));
        assert!(*ran.lock().unwrap(), "must run once the delay has elapsed");
    }

    #[test]
    fn a_burst_of_calls_runs_on_settled_exactly_once_using_the_last_call() {
        let counter = fresh_counter();
        let settled_with = Arc::new(Mutex::new(Vec::<u32>::new()));

        // Simulate rapid keystrokes: several debounce() calls well inside
        // each other's delay window. Only the LAST one should ever run
        // its on_settled closure.
        for i in 0..5u32 {
            let settled_with = settled_with.clone();
            debounce(counter, Duration::from_millis(60), move || {
                settled_with.lock().unwrap().push(i);
            });
            std::thread::sleep(Duration::from_millis(10));
        }

        // Wait well past the last call's own delay window.
        std::thread::sleep(Duration::from_millis(100));

        let settled = settled_with.lock().unwrap();
        assert_eq!(
            settled.as_slice(),
            &[4],
            "exactly one settlement, from the LAST call in the burst (index 4), not any \
             earlier superseded one and not more than one"
        );
    }

    #[test]
    fn two_calls_spaced_further_apart_than_the_delay_both_settle() {
        let counter = fresh_counter();
        let settled_with = Arc::new(Mutex::new(Vec::<u32>::new()));

        for i in 0..2u32 {
            let settled_with = settled_with.clone();
            debounce(counter, Duration::from_millis(30), move || {
                settled_with.lock().unwrap().push(i);
            });
            // Longer than the delay -- the first call must be allowed to
            // settle before the second one is even scheduled.
            std::thread::sleep(Duration::from_millis(60));
        }

        let settled = settled_with.lock().unwrap();
        assert_eq!(
            settled.as_slice(),
            &[0, 1],
            "calls spaced apart by more than the delay must each settle independently -- \
             debouncing an isolated edit must not silently swallow it"
        );
    }
}
