//! Effect execution boundary for the TEA dispatcher.
//!
//! `update()` describes side effects; this module is the only production
//! code that executes those descriptions. Results re-enter through
//! `Msg::Effect`, while bridge/store snapshots re-enter through `Msg::Frame`.

use crate::dispatch::update_persistent;
use crate::effect::{Effect, EffectError, EffectResultMsg};
use crate::msg::{FrameInput, Msg};
use crate::PanelSingleton;
use slint::ComponentHandle;

/// Default frame poll does not collect skills. After filesystem skill
/// mutations, re-scan and fold an explicit skills snapshot so the list
/// stays in sync without a dual-path `refresh_skills_model`.
fn refresh_skills_after_effect(panel: &PanelSingleton) {
    let skills_snapshot = crate::external_snapshot::ExternalSnapshotSource::new(panel)
        .collect_skills_snapshot();
    panel.dispatch_frame_input(FrameInput {
        skills_snapshot: Some(skills_snapshot),
        ..FrameInput::default()
    });
}

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
                            // Content write does not change list identity; no skills rescan.
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
                        let content = std::fs::read_to_string(path.join("SKILL.md"))
                            .map_err(|error| EffectError::new(error.to_string()))?;
                        let detected_editors = crate::editor_detect::detect_installed_editors()
                            .into_iter()
                            .map(str::to_owned)
                            .collect();
                        Ok(crate::model::SkillEditorState {
                            name,
                            path: path.to_string_lossy().into_owned(),
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
                            refresh_skills_after_effect(panel);
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
            Effect::SendPrompt { real_index, text } => {
                panel.execute_send_prompt_real(real_index, &text);
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
            Effect::McpServerAuthenticate { name, .. } => {
                let Some(component) = panel.component.as_weak().upgrade() else {
                    continue;
                };
                panel.dispatch_mcp_server_authenticate(&component, &name);
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
                panel.dispatch_mcp_server_tool_enabled_changed(
                    &component,
                    &server_name,
                    &tool_name,
                    enabled,
                );
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
            Effect::CloseThread { real_index } => {
                if let Some(bridge) = panel.bridge.as_ref() {
                    // The actual wiring for this thread's own "background"
                    // toggle (previously stored and displayed, but never
                    // connected to any real close-session behavior) --
                    // see AgentBridge::close_thread's doc comment.
                    let thread_id = bridge
                        .thread_binding(real_index)
                        .map(|binding| binding.thread_id);
                    let background = thread_id
                        .as_ref()
                        .and_then(|thread_id| {
                            panel
                                .panel_state
                                .as_ref()
                                .map(|store| (store, thread_id))
                        })
                        .and_then(|(store, thread_id)| {
                            store.effective_background_session(thread_id).ok()
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
            Effect::ArchiveThread { real_index } => {
                if let Some(bridge) = panel.bridge.as_ref() {
                    let thread_id = bridge
                        .thread_binding(real_index)
                        .map(|binding| binding.thread_id)
                        .unwrap_or_default();
                    if !bridge.archive_thread(real_index) {
                        let message = format!("failed to archive thread {real_index}");
                        eprintln!("panel-rust: {message}");
                        let _ = update_persistent(
                            panel,
                            Msg::Effect(EffectResultMsg::StateEffectFailed { thread_id, message }),
                        );
                    }
                }
            }
            Effect::PersistSelectedThread { thread_id } => {
                if let Some(store) = panel.panel_state.as_ref() {
                    if let Err(error) = store.set_selected_thread_id(Some(&thread_id)) {
                        let message = format!("failed to persist selected chat thread: {error}");
                        eprintln!("panel-rust: {message}");
                        let _ = update_persistent(
                            panel,
                            Msg::Effect(EffectResultMsg::StateEffectFailed { thread_id, message }),
                        );
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
                    let message = format!("failed to toggle background-session override: {error}");
                    eprintln!("panel-rust: {message}");
                    let _ = update_persistent(
                        panel,
                        Msg::Effect(EffectResultMsg::StateEffectFailed { thread_id, message }),
                    );
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
                let result = crate::external_snapshot::ExternalSnapshotSource::new(panel)
                    .collect_thread_record(real_index)
                    .map(|record| {
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
                let thread_id = panel
                    .model
                    .borrow()
                    .threads
                    .get(real_index)
                    .map(|thread| thread.thread_id.clone());
                if let (Some(store), Some(thread_id)) = (panel.panel_state.as_ref(), thread_id) {
                    if let Err(error) = store.update_thread_display_name(&thread_id, &name) {
                        let message = format!("failed to persist renamed chat thread: {error}");
                        eprintln!("panel-rust: {message}");
                        let _ = update_persistent(
                            panel,
                            Msg::Effect(EffectResultMsg::StateEffectFailed { thread_id, message }),
                        );
                    }
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
                let result = std::fs::remove_dir_all(&path)
                    .map_err(|error| EffectError::new(error.to_string()));
                if let Err(error) = &result {
                    eprintln!("panel-rust: failed to delete skill: {error}");
                }
                let _ = update_persistent(
                    panel,
                    Msg::Effect(match result {
                        Ok(()) => EffectResultMsg::SkillWritten(Ok(())),
                        Err(error) => EffectResultMsg::SkillWritten(Err(error)),
                    }),
                );
                refresh_skills_after_effect(panel);
            }
            Effect::NewThread { .. } | Effect::RecoverSessionAttach { .. } => {
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

fn write_clipboard_text(text: &str) -> Result<(), String> {
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
