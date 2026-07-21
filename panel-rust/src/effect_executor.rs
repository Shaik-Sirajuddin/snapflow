//! Effect execution boundary for the TEA dispatcher.
//!
//! `update()` describes side effects; this module is the only production
//! code that executes those descriptions. Results re-enter through
//! `Msg::Effect`, while bridge/store snapshots re-enter through `Msg::Frame`.

use crate::dispatch::update_persistent;
use crate::effect::{Effect, EffectError, EffectResultMsg};
use crate::msg::Msg;
use crate::PanelSingleton;
use slint::ComponentHandle;

fn execute_skill_effects(effects: Vec<Effect>) {
    for effect in effects {
        match effect {
            Effect::SkillWrite { path, content } => {
                std::thread::spawn(move || {
                    let result = std::fs::write(path, content)
                        .map_err(|error| EffectError::new(error.to_string()));
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
                            panel.dispatch_frame_input(crate::msg::FrameInput {
                                skills_snapshot: Some(panel.collect_skills_snapshot()),
                                ..crate::msg::FrameInput::default()
                            });
                        });
                    });
                });
            }
            Effect::SkillPromoteToGlobal { path } => {
                std::thread::spawn(move || {
                    let cache_dir = crate::resolve_cache_dir();
                    let global_dir = crate::skills_state::global_skills_dir(&cache_dir);
                    let result = crate::skills_state::promote_skill_to_global(&path, &global_dir)
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
                            panel.dispatch_frame_input(crate::msg::FrameInput {
                                skills_snapshot: Some(panel.collect_skills_snapshot()),
                                ..crate::msg::FrameInput::default()
                            });
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
    for effect in effects {
        match effect {
            Effect::LoadInitialState => {}
            Effect::SendPrompt { real_index, text } => {
                panel.execute_send_prompt_real(real_index, &text);
            }
            Effect::CancelGeneration { real_index } => {
                panel.execute_cancel_generation_real(real_index);
            }
            Effect::RespondAgentRequest { approve, .. } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                panel.answer_pending_request(&component, approve);
            }
            Effect::PermissionOptionSelected { option, .. } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                panel.answer_pending_request_option(&component, &option);
            }
            Effect::LoadOlderMessages { .. } => {
                panel.dispatch_load_older_requested();
            }
            Effect::LocalTerminalSpawn => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                panel.dispatch_local_terminal_toggle(&component);
            }
            Effect::LocalTerminalKill => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                panel.dispatch_local_terminal_close(&component);
            }
            Effect::LocalTerminalWrite { bytes } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                let text = String::from_utf8_lossy(&bytes);
                panel.dispatch_local_terminal_key_input(&component, &text);
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
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                panel.dispatch_config_option_selected(&component, &key, &value);
            }
            Effect::SetMode { mode, .. } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                panel.dispatch_mode_selected(&component, &mode);
            }
            Effect::SaveDevMode { enabled } => {
                panel.dispatch_dev_mode_toggled(enabled);
            }
            Effect::McpServerCreate { name, command, .. } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                panel.dispatch_mcp_server_create(&component, &name, &command);
            }
            Effect::McpServerDelete { name, .. } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                panel.dispatch_mcp_server_delete(&component, &name);
            }
            Effect::McpServerEnabledChanged { name, enabled, .. } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                panel.dispatch_mcp_server_enabled_changed(&component, &name, enabled);
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
            Effect::SkillWrite { .. } | Effect::SkillPromoteToGlobal { .. } => {
                execute_skill_effects(vec![effect]);
            }
            Effect::SetActiveProjectPath { path } => {
                panel.apply_active_project_path(path);
            }
            Effect::CloseThread { real_index } => {
                if let Some(bridge) = panel.bridge.as_ref() {
                    let _ = bridge.close_thread(real_index);
                }
            }
            Effect::PersistSelectedThread { thread_id } => {
                if let Some(store) = panel.panel_state.as_ref() {
                    if let Err(error) = store.set_selected_thread_id(Some(&thread_id)) {
                        eprintln!("panel-rust: failed to persist selected chat thread: {error}");
                    }
                }
            }
            Effect::ToggleBackground { real_index } => {
                let Some(store) = panel.panel_state.as_ref() else {
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
                let next = !store
                    .effective_background_session(&thread_id)
                    .unwrap_or(false);
                if let Err(error) = store.set_background_override(&thread_id, Some(next)) {
                    eprintln!("panel-rust: failed to toggle background-session override: {error}");
                }
            }
            Effect::PersistThreadRecord { record } => {
                let result = panel
                    .panel_state
                    .as_ref()
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
            }
            Effect::PersistThread { real_index } => {
                let result = panel.collect_thread_record(real_index).map(|record| {
                    panel
                        .panel_state
                        .as_ref()
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
                            Msg::Effect(EffectResultMsg::ThreadPersisted { real_index, result }),
                        );
                    });
                });
            }
            Effect::RenameThread { real_index, name } => {
                if let (Some(store), Some(thread_id)) = (
                    panel.panel_state.as_ref(),
                    panel
                        .model
                        .borrow()
                        .threads
                        .get(real_index)
                        .map(|thread| thread.thread_id.clone()),
                ) {
                    if let Err(error) = store.update_thread_display_name(&thread_id, &name) {
                        eprintln!("panel-rust: failed to persist renamed chat thread: {error}");
                    }
                }
            }
            Effect::DeleteThread { real_index } => {
                if let Some(bridge) = panel.bridge.as_ref() {
                    let _ = bridge.delete_thread(real_index);
                }
            }
            Effect::SkillDelete { path } => {
                if let Err(error) = std::fs::remove_dir_all(path) {
                    eprintln!("panel-rust: failed to delete skill: {error}");
                }
            }
            Effect::NewThread { .. } | Effect::RecoverSessionAttach { .. } => {
                debug_assert!(
                    false,
                    "thread lifecycle effects must use execute_thread_lifecycle_effect"
                );
            }
        }
    }
}
