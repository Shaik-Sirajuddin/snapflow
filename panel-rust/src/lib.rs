//! Phase 4: real chat panel, wired to `rui-acp-client` for genuine
//! ACP-backed session data via [`agent_bridge::AgentBridge`] -- no more
//! static placeholders (phase 2 had layout-only, static arrays; see git
//! history / the phase-2 state doc for that snapshot). Rendered by the
//! same proven render bridge from phase 1
//! (rust-qt-cross-render-option-b.md).
//!
//! Threading note (see phase 1 finding, and `agent_bridge`'s module docs
//! for how phase 4 respects it): the whole Slint side must stay on one OS
//! thread. This process must be launched with `QSG_RENDER_LOOP=basic` so
//! Qt's paint() and input dispatch share a thread -- otherwise this
//! thread_local singleton silently forks into two never-synchronized
//! copies (confirmed the hard way in phase 1). The agent bridge's
//! background tokio runtime runs on its own worker threads but never
//! touches Slint state directly -- see `agent_bridge.rs`.

mod agent_bridge;
mod appearance;
mod conversation;
mod editor_detect;
pub mod gateway_actor;
pub mod jsonl_store;
mod local_terminal;
mod markdown;
mod models;
mod permission;
pub mod protocol_types;
mod send_queue;
mod settings_file;
mod skills_state;
mod state_store;
mod theme;

use agent_bridge::{resolve_cache_dir, AgentBridge, ThreadSpec};
use appearance::{AppearanceState, ColorScheme, HostAppearance};
use models::{build_thread_items, describe_thread, to_message_model_from_transcript, ThreadState};
use protocol_types::{AgentEvent, ChatMessage, MessageKind};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Key, Platform, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{ModelRc, SharedString, VecModel};
use state_store::{PanelDefaults, PanelStateStore, ThreadRecord};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::os::raw::{c_int, c_uchar, c_uint};
use std::rc::Rc;

/// Truncation length for `models::describe_thread`'s sidebar preview --
/// matches the HTML source's short one-liners (e.g. "Trim clips and add
/// fades…").
const THREAD_DESCRIPTION_MAX_CHARS: usize = 48;

/// Fixed v1 set of chat threads -- each gets its own bound agent
/// connection via `AgentBridge` (Decision 4: per-thread static binding).
/// A dynamic thread list (create/rename/delete threads from the UI) is
/// follow-up work, not built here.
const DEFAULT_THREAD_NAMES: &[&str] = &[
    "Fix timeline crash",
    "Add fade transition",
    "Refactor filters",
    "Export pipeline bug",
];

/// Maps a Qt key event (`QKeyEvent::key()`'s `int` plus `QKeyEvent::text()`)
/// to a Slint key-event `SharedString`. Qt::Key special codes below are the
/// stable `qnamespace.h` values for the handful of editing/navigation keys
/// a single-line chat compose box needs; anything else falls back to the
/// already-localized `text` Qt hands us (correct for regular printable
/// input, including non-ASCII layouts -- Qt has already done the keymap
/// work by the time `text()` is populated). Returns `None` for pure
/// modifier presses (empty text, no special mapping) which Slint doesn't
/// need forwarded as a `KeyPressed`/`KeyReleased` text event.
fn map_qt_key(qt_key: c_int, text: &str) -> Option<SharedString> {
    let special = match qt_key {
        0x0100_0000 => Some(Key::Escape),
        0x0100_0001 => Some(Key::Tab),
        0x0100_0003 => Some(Key::Backspace),
        0x0100_0004 | 0x0100_0005 => Some(Key::Return),
        0x0100_0007 => Some(Key::Delete),
        0x0100_0010 => Some(Key::Home),
        0x0100_0011 => Some(Key::End),
        0x0100_0012 => Some(Key::LeftArrow),
        0x0100_0013 => Some(Key::UpArrow),
        0x0100_0014 => Some(Key::RightArrow),
        0x0100_0015 => Some(Key::DownArrow),
        _ => None,
    };
    if let Some(k) = special {
        return Some(k.into());
    }
    if text.is_empty() {
        // QQuickItem receives an empty `QKeyEvent::text()` for some printable
        // keys when the host also owns a shortcut for that key. Qt still
        // provides the ASCII `Qt::Key_*` code, so recover that character for
        // a focused composer instead of letting host shortcuts eat the input.
        // Shifted/non-ASCII input continues to use Qt's non-empty text path.
        match u32::try_from(qt_key)
            .ok()
            .and_then(char::from_u32)
            .filter(|ch| ch.is_ascii_graphic() || *ch == ' ')
        {
            Some(ch) if ch.is_ascii_uppercase() => Some(ch.to_ascii_lowercase().into()),
            Some(ch) => Some(ch.into()),
            None => None,
        }
    } else {
        Some(SharedString::from(text))
    }
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

/// One-shot seed: if the global JSON file is missing but SQLite still has
/// panel prefs, write them so multi-process peers can read the same values.
fn maybe_migrate_sqlite_defaults_to_json(store: &PanelStateStore) {
    let paths = settings_file::SettingsPaths::from_env();
    if paths.global.exists() {
        return;
    }
    let Ok(defaults) = store.defaults() else {
        return;
    };
    let has_prefs = defaults.profile_name.is_some()
        || defaults.permission_profile.is_some()
        || defaults.background_session;
    if !has_prefs {
        return;
    }
    let doc = settings_file::SettingsDocument {
        schema_version: 1,
        default_profile: defaults.profile_name,
        permission_profile: defaults.permission_profile,
        background_session_default: Some(defaults.background_session),
        default_agent_id: None,
        harness: None,
        dev_mode: None,
    };
    if let Err(error) = settings_file::save_document(&paths.global, &doc) {
        eprintln!("panel-rust: failed to migrate panel defaults to JSON: {error}");
    }
}

/// Load multi-process panel prefs from JSON (project → global → default).
/// `selected_thread_id` remains process-local (SQLite) when provided.
fn load_panel_prefs(selected_thread_id: Option<String>) -> PanelDefaults {
    let paths = settings_file::SettingsPaths::from_env();
    match paths.load_resolved() {
        Ok(resolved) => {
            settings_file::resolved_to_panel_defaults(&resolved, selected_thread_id)
        }
        Err(error) => {
            eprintln!("panel-rust: settings file load failed: {error}");
            PanelDefaults {
                selected_thread_id,
                ..PanelDefaults::default()
            }
        }
    }
}

/// Persist profile / permission / background-default into the global JSON
/// file (atomic write). Preserves other keys already on disk (harness, …).
fn save_panel_prefs_to_json(defaults: &PanelDefaults) -> Result<(), settings_file::SettingsFileError> {
    let paths = settings_file::SettingsPaths::from_env();
    let mut doc = settings_file::load_document(&paths.global).unwrap_or_default();
    doc.schema_version = 1;
    doc.default_profile = defaults.profile_name.clone();
    doc.permission_profile = defaults.permission_profile.clone();
    doc.background_session_default = Some(defaults.background_session);
    settings_file::save_document(&paths.global, &doc)
}

/// Opt-in host-event diagnostics for the real-process harness. Disabled by
/// default because key text may be sensitive; when enabled, this writes only
/// to Shotcut's stderr and never changes input routing.
fn trace_host_input(message: impl std::fmt::Display) {
    if std::env::var_os("RUI_PANEL_INPUT_TRACE").is_some() {
        eprintln!("panel-rust input: {message}");
    }
}

// Slint UI markup moved to `panel-rust/ui/*.slint` (Phase 1 of
// chat-panel-ui-theme-parity.md's modularity requirement) -- compiled by
// `build.rs` via `slint_build::compile`. `ChatPanel`, `ThreadItem`, and
// `MessageItem` below are the same generated Rust bindings the inline
// `slint::slint! { ... }` macro used to produce; nothing downstream in
// this file needed to change.
slint::include_modules!();

struct SpikePlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for SpikePlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

struct PanelSingleton {
    window: Rc<MinimalSoftwareWindow>,
    component: ChatPanel,
    buffer: RefCell<Vec<PremultipliedRgbaColor>>,
    width: u32,
    height: u32,
    bridge: Option<AgentBridge>,
    panel_state: Option<PanelStateStore>,
    appearance: RefCell<AppearanceState>,
    /// `active_project_binding` phase: the currently-open Shotcut MLT
    /// project's path, pushed in from the C++ host via
    /// `panel_rust_set_project_path` (mirroring `panel_rust_set_theme`'s
    /// byte-buffer FFI shape) whenever `MainWindow::producerOpened`
    /// fires. `None` before the first project opens, or if Shotcut has
    /// no project open. This is deliberately just storage for now --
    /// `thread_item_project_context`/`chat_sessions_project_path`
    /// consume it.
    active_project_path: RefCell<Option<String>>,
    thread_names: RefCell<Vec<String>>,
    /// Immutable ACPX profile bindings, held alongside the display names so
    /// a background session attachment can persist a complete `ThreadRecord`
    /// as soon as its session ID becomes available.
    thread_profiles: RefCell<Vec<Option<String>>>,
    thread_permission_profiles: RefCell<Vec<Option<String>>>,
    /// Host-test trace deduplication for asynchronous attachment readiness.
    /// This never affects persistence or routing.
    traced_attachment_threads: RefCell<HashSet<String>>,
    thread_state: RefCell<Vec<ThreadState>>,
    thread_errors: RefCell<Vec<String>>,
    /// `queued_send_queue_behavior` phase: one `SendQueue` per thread,
    /// parallel to `thread_state`/`thread_names` (same real_index
    /// convention, grown at exactly the two thread-creation call sites
    /// that also grow those). In-memory only for now -- restart
    /// persistence (`SendQueue::load`/JSONL) needs a real per-thread
    /// identity available at construction time, which isn't wired up
    /// yet; this still gets the core behavior (always-typeable input,
    /// correct enqueue/drain) without gambling on that extra plumbing
    /// in the same pass.
    send_queues: RefCell<Vec<crate::send_queue::SendQueue>>,
    /// Phase 2 (chat-panel-ui-theme-parity.md): current sidebar search
    /// filter, empty means "show all".
    search_query: RefCell<String>,
    /// Maps each currently-*visible* (post-filter) row index back to its
    /// real index into `thread_names`/`thread_state`/the agent bridge --
    /// filtering means `threads[i]` in Slint is no longer the same `i` as
    /// `bridge.history(i)`; every Rust-side handler that receives a
    /// `selected-thread`/`thread-selected(i)` value from Slint must
    /// translate it through this map first (`real_index`). Rebuilt by
    /// `refresh_threads_model` every time the filter or thread_state
    /// changes.
    visible_indices: RefCell<Vec<usize>>,
    /// Phase 3 (chat-panel-ui-theme-parity.md): UI-only collapse state
    /// for tool-call log bodies, parallel to whichever thread's messages
    /// are currently displayed -- see `refresh_messages_for`/
    /// `render_messages` below. Does not persist across a thread switch
    /// or a jsonl reload (render concern only, not part of
    /// `ChatMessage`).
    expanded: RefCell<Vec<bool>>,
    /// The real thread index whose history `expanded` currently
    /// describes -- `None` before the first message render. Used to
    /// decide whether switching to `real_idx` should reset `expanded`
    /// (different thread) or just grow it in place (same thread, new
    /// streamed messages).
    displayed_thread: Cell<Option<usize>>,
    /// Terminal-view addition: which terminal id (if any) the floating
    /// overlay is currently showing -- `None` means closed. Set by the
    /// `expand-terminal` callback, cleared by `close-terminal-overlay`;
    /// re-read every refresh so the overlay keeps showing live output
    /// while open (same "Rust owns the source of truth, Slint property
    /// is just a snapshot" convention `refresh_pending_request_for`
    /// already follows).
    expanded_terminal_id: RefCell<Option<String>>,
    /// Last-rendered client-local terminal screen text, for `refresh_
    /// local_terminal_for`'s change-detection -- see that method's doc
    /// comment.
    local_terminal_last_text: RefCell<String>,
    /// Set by [`settings_file::SettingsWatcher`]; drained on poll to
    /// refresh open settings fields without clobbering dirty edits.
    settings_reload_pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Suppress self-write feedback from settings save for a short window.
    settings_ignore_watch_until: Cell<Option<std::time::Instant>>,
    _settings_watcher: Option<settings_file::SettingsWatcher>,
}

impl PanelSingleton {
    /// Gateway index for settings RPCs: selected real thread, else first
    /// bound thread, else `0` only as last resort when the bridge exists.
    fn settings_gateway_index(&self) -> usize {
        if let Some(idx) = self.real_index(self.component.get_selected_thread() as usize) {
            if self
                .bridge
                .as_ref()
                .and_then(|b| b.thread_binding(idx))
                .is_some()
            {
                return idx;
            }
        }
        let n = self.thread_names.borrow().len();
        if let Some(bridge) = self.bridge.as_ref() {
            for idx in 0..n {
                if bridge.thread_binding(idx).is_some() {
                    return idx;
                }
            }
        }
        0
    }

    /// Refresh gateway-backed settings lists (profiles / MCP / agents /
    /// recoverable sessions) using [`Self::settings_gateway_index`].
    fn refresh_settings_gateway_lists(&self) {
        let Some(bridge) = &self.bridge else {
            self.component
                .set_available_profiles(models::to_profile_options(vec![]));
            self.component
                .set_available_mcp_servers(models::to_mcp_server_options(vec![]));
            self.component
                .set_agent_catalog(models::to_agent_catalog_entries(vec![]));
            self.component
                .set_recoverable_sessions(models::to_remote_session_options(vec![], ""));
            return;
        };
        let gw = self.settings_gateway_index();
        self.component
            .set_available_profiles(models::to_profile_options(bridge.list_profiles(gw)));
        self.component.set_available_mcp_servers(models::to_mcp_server_options(
            bridge.list_mcp_servers(gw),
        ));
        self.component
            .set_agent_catalog(models::to_agent_catalog_entries(bridge.list_agents(gw)));
        let recovery_provider = bridge.thread_provider(gw).unwrap_or_default();
        self.component.set_recoverable_sessions(models::to_remote_session_options(
            bridge.recoverable_sessions(gw),
            &recovery_provider,
        ));
    }

    fn apply_json_prefs_to_component(&self) {
        let defaults = load_panel_prefs(None);
        self.component
            .set_default_profile(defaults.profile_name.unwrap_or_default().into());
        self.component.set_permission_profile(
            defaults.permission_profile.unwrap_or_default().into(),
        );
        self.component
            .set_background_default(defaults.background_session);
    }

    /// Derives a conservative PTY grid from the actual dock viewport.
    /// The client terminal remains bounded in its card, but its backend
    /// process must still receive a real resize whenever the host changes
    /// the panel geometry.
    fn local_terminal_dimensions(&self) -> (u16, u16) {
        let cols = (self.width / 8).clamp(20, 240) as u16;
        let rows = (self.height / 18).clamp(8, 120) as u16;
        (cols, rows)
    }

    fn resize_local_terminals_for_viewport(&self) {
        let Some(bridge) = &self.bridge else { return };
        let (cols, rows) = self.local_terminal_dimensions();
        for idx in 0..self.thread_names.borrow().len() {
            if bridge.has_local_terminal(idx) {
                bridge.resize_local_terminal(idx, cols, rows);
            }
        }
    }

    /// Persist thread identity only after the asynchronous ACPX attachment
    /// has supplied a concrete session ID. Creation deliberately returns
    /// before that attachment finishes so cached UI can render immediately;
    /// attempting this work only during creation silently skipped every
    /// initial record and made the next host process unable to reattach.
    fn sync_thread_records(&self) {
        let (Some(store), Some(bridge)) = (&self.panel_state, &self.bridge) else {
            return;
        };
        let names = self.thread_names.borrow();
        let profiles = self.thread_profiles.borrow();
        let permission_profiles = self.thread_permission_profiles.borrow();
        for idx in 0..names.len() {
            let Some(binding) = bridge.thread_binding(idx) else {
                continue;
            };
            let Some(provider) = bridge.thread_provider(idx) else {
                continue;
            };
            let record = ThreadRecord {
                thread_id: binding.thread_id,
                display_name: names[idx].clone(),
                provider,
                session_id: binding.session_id,
                profile_name: profiles.get(idx).cloned().flatten(),
                permission_profile: permission_profiles.get(idx).cloned().flatten(),
                background_session: None,
            };
            match store.save_thread_record(&record) {
                Ok(()) => {
                    if self
                        .traced_attachment_threads
                        .borrow_mut()
                        .insert(record.thread_id.clone())
                    {
                        trace_host_input(format_args!(
                            "attachment ready thread={idx} session={:?}",
                            record.session_id
                        ));
                    }
                }
                Err(error) => {
                    // A record may already have a deliberately immutable
                    // profile/permission binding. Keep the live session usable
                    // and leave that durable identity untouched.
                    eprintln!("panel-rust: failed to persist chat thread binding: {error}");
                }
            }
        }
    }

    /// Rebuilds and pushes the `threads` model from the dynamic thread list +
    /// current `thread_state`, narrowed by `search_query` (Phase 2's
    /// real client-side filter -- see `models::build_thread_items`).
    /// Called any time a thread's status changes (send in flight, turn
    /// ended, error) or the search box is edited.
    fn refresh_threads_model(&self) {
        let state = self.thread_state.borrow();
        let query = self.search_query.borrow();
        // Phase 3: sidebar description is synthesized from each
        // thread's latest cached message (`models::describe_thread`) --
        // recomputed here rather than cached, since it must track the
        // live/bridge history, not just `thread_state`.
        let names = self.thread_names.borrow();
        let errors = self.thread_errors.borrow();
        let descriptions: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(i, _)| {
                if let Some(error) = errors.get(i).filter(|error| !error.is_empty()) {
                    return format!("Error: {error}");
                }
                match &self.bridge {
                    Some(bridge) => {
                        describe_thread(&bridge.history(i), THREAD_DESCRIPTION_MAX_CHARS)
                    }
                    None => String::new(),
                }
            })
            .collect();
        let background_sessions: Vec<bool> = names
            .iter()
            .enumerate()
            .map(|(idx, _)| {
                let Some(store) = self.panel_state.as_ref() else {
                    return false;
                };
                let Some(thread_id) = self
                    .bridge
                    .as_ref()
                    .and_then(|bridge| bridge.thread_binding(idx))
                    .map(|binding| binding.thread_id)
                else {
                    return false;
                };
                store.effective_background_session(&thread_id).unwrap_or(false)
            })
            .collect();
        let closed: Vec<bool> = names
            .iter()
            .enumerate()
            .map(|(idx, _)| {
                self.bridge
                    .as_ref()
                    .map(|bridge| bridge.thread_closed(idx))
                    .unwrap_or(false)
            })
            .collect();
        let providers: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(idx, _)| {
                self.bridge
                    .as_ref()
                    .and_then(|bridge| bridge.thread_provider(idx))
                    .unwrap_or_default()
            })
            .collect();
        let thread_models: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(idx, _)| {
                self.bridge
                    .as_ref()
                    .map(|bridge| models::model_name_from_config(&bridge.config_options(idx)))
                    .unwrap_or_default()
            })
            .collect();
        let thread_project_paths: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(idx, _)| {
                self.bridge
                    .as_ref()
                    .and_then(|bridge| bridge.thread_project_path(idx))
                    .unwrap_or_default()
            })
            .collect();
        let items = build_thread_items(
            &*names,
            &state,
            &descriptions,
            &background_sessions,
            &closed,
            &query,
        );
        *self.visible_indices.borrow_mut() = items.iter().map(|i| i.real_index).collect();
        let items: Vec<ThreadItem> = items
            .into_iter()
            .map(|i| {
                let mut item = i.item;
                item.provider = providers.get(i.real_index).cloned().unwrap_or_default().into();
                item.model = thread_models.get(i.real_index).cloned().unwrap_or_default().into();
                let project_path = thread_project_paths
                    .get(i.real_index)
                    .cloned()
                    .unwrap_or_default();
                item.project_name = std::path::Path::new(&project_path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .into();
                item.project_path = project_path.into();
                item
            })
            .collect();
        self.component
            .set_threads(ModelRc::new(VecModel::from(items)));
    }

    /// Rebuilds the `skills` sidebar model from the global skills
    /// directory (`skill_discovery_backend` phase). Project-local
    /// scanning is deliberately not wired here yet -- it needs
    /// `active_project_binding`'s active-project state, which doesn't
    /// exist yet -- so this only ever reports global skills for now.
    fn refresh_skills_model(&self) {
        let global_dir = crate::skills_state::global_skills_dir(&resolve_cache_dir());
        let mut entries = crate::skills_state::scan_skills_dir(
            &global_dir,
            crate::skills_state::SkillScope::Global,
        );
        // `project_scoped_skill_isolation`: now that `active_project_binding`
        // is real, also scan the active project's own `.skills/` directory
        // -- entirely additive to the always-scanned global directory, and
        // naturally empty (not an error) when no project is open or it has
        // no `.skills/` yet, since `scan_skills_dir` already treats a
        // missing directory as an empty result.
        if let Some(project_path) = self.active_project_path.borrow().as_ref() {
            // `active_project_path` is the open MLT *file*'s path
            // (`MainWindow::fileName()`), not its containing directory --
            // `.skills/` lives alongside the project file, so this needs
            // the parent directory.
            if let Some(project_dir) = std::path::Path::new(project_path).parent() {
                let skills_dir = crate::skills_state::project_skills_dir(project_dir);
                entries.extend(crate::skills_state::scan_skills_dir(
                    &skills_dir,
                    crate::skills_state::SkillScope::Project,
                ));
            }
        }
        self.component
            .set_available_skills(crate::models::to_skill_options(entries));
    }

    /// Translates a Slint-side filtered-list index (what `thread-selected`
    /// callbacks and `get_selected_thread()` hand back) into the real
    /// index the agent bridge/`thread_state` use. `None` if out of range
    /// (e.g. the filter just emptied the list out from under a stale
    /// selection).
    fn real_index(&self, filtered_idx: usize) -> Option<usize> {
        self.visible_indices.borrow().get(filtered_idx).copied()
    }

    /// Rebuilds the `messages` model for `real_idx` from the agent
    /// bridge's current history plus whatever `expanded` state already
    /// exists -- does not touch `expanded`/`displayed_thread` itself
    /// (that's `refresh_messages_for`'s job). Used by the
    /// `toggle-expanded` callback, which only flips one bool and must
    /// not reset collapse state for every other message in the thread.
    fn render_messages(&self, real_idx: usize) {
        let Some(bridge) = &self.bridge else { return };
        // Phase 2 step 3: render the *merged* transcript view
        // (`AgentBridge::transcript`, streamed chunks/tool-status
        // updates already merged by id), not the raw per-chunk
        // `history` feed -- see `models::to_message_model_from_
        // transcript`'s doc comment.
        let transcript = bridge.transcript(real_idx);
        // Coverage-matrix "tool stream" host scenario: a compact,
        // opt-in trace of the *typed reducer transcript*'s own tail
        // (kind + a truncated text preview per entry), so a host test
        // can confirm the rendered thought/tool-call/message
        // discriminator sequence and content without a screenshot --
        // this is the same Slint model `to_message_model_from_
        // transcript` below turns into `MessageItem`s, just observed
        // from the Rust side instead of the render tree.
        if std::env::var_os("RUI_PANEL_INPUT_TRACE").is_some() {
            use crate::conversation::TranscriptItem;
            let tail_start = transcript.len().saturating_sub(3);
            for (offset, entry) in transcript[tail_start..].iter().enumerate() {
                let (kind, raw_text) = match entry {
                    TranscriptItem::User { text, .. } => ("user", text.as_str()),
                    TranscriptItem::Assistant { text, .. } => ("agent", text.as_str()),
                    TranscriptItem::Thought { text, .. } => ("thinking", text.as_str()),
                    TranscriptItem::Tool { title, .. } => ("tool-call", title.as_str()),
                    TranscriptItem::Terminal { title, .. } => ("terminal", title.as_str()),
                    TranscriptItem::Notice { text, .. } => ("notice", text.as_str()),
                };
                let preview: String = raw_text.chars().take(60).collect();
                let preview = preview.replace('\n', " ");
                trace_host_input(format_args!(
                    "transcript thread={real_idx} index={} kind={kind} text={preview:?}",
                    tail_start + offset,
                ));
            }
        }
        let expanded = self.expanded.borrow();
        self.component
            .set_messages(to_message_model_from_transcript(transcript, &expanded));
        // Phase 3 step 2: whether another predecessor page exists.
        self.component
            .set_has_older_messages(bridge.has_older_page(real_idx));
    }

    /// Displays `real_idx`'s messages, first reconciling `expanded`
    /// against it: a genuine thread switch (different from
    /// `displayed_thread`) resets collapse state to all-collapsed
    /// (matches the HTML source's "new tool_use items default to
    /// collapsed" convention); staying on the same thread (e.g. a
    /// streamed message just arrived, growing history by one) only
    /// grows the vec, preserving whatever the user already
    /// expanded/collapsed. Every Rust-side call site that changes which
    /// thread's messages are visible goes through this, not
    /// `set_messages` directly.
    fn refresh_messages_for(&self, real_idx: usize) {
        let Some(bridge) = &self.bridge else { return };
        // Sized against the *merged* transcript's row count, not raw
        // `history`'s chunk count -- `toggle-expanded(index)` callbacks
        // (see `on_toggle_expanded` below) index into this same vec by
        // the row index `render_messages`'s `MessageItem::index` field
        // assigns, which is a transcript row index post-merge.
        let history_len = bridge.transcript(real_idx).len();
        let is_thread_switch = self.displayed_thread.get() != Some(real_idx);
        {
            let mut expanded = self.expanded.borrow_mut();
            if is_thread_switch {
                *expanded = vec![false; history_len];
            } else if expanded.len() < history_len {
                expanded.resize(history_len, false);
            }
        }
        self.displayed_thread.set(Some(real_idx));
        self.render_messages(real_idx);
        self.refresh_pending_request_for(real_idx);
        self.refresh_terminals_for(real_idx);
        self.refresh_capabilities_for(real_idx);
        self.refresh_local_terminal_for(real_idx);
        self.refresh_connection_status_for(real_idx);
    }

    fn refresh_connection_status_for(&self, real_idx: usize) -> bool {
        let status = self
            .bridge
            .as_ref()
            .map(|bridge| bridge.transport_status(real_idx))
            .unwrap_or_else(|| "Unavailable".to_owned());
        let changed = self.component.get_connection_status().as_str() != status;
        if changed {
            self.component.set_connection_status(status.into());
        }
        changed
    }

    /// Rebuilds the `available-modes`/`current-mode-id`/`config-option-
    /// rows` properties for `real_idx` from `AgentBridge::session_modes`/
    /// `config_options` -- see [`Self::refresh_terminals_for`]'s doc
    /// comment for the shared "this thread became the displayed one"
    /// hook convention this follows. Both a genuine `session/new`-time
    /// advertisement and any later live `current_mode_update`/`config_
    /// option_update` notification reach the UI purely by this being
    /// re-called on every event that touches the selected thread (see
    /// `apply_bridge_events`'s `AgentEvent::SessionModes`/`Current
    /// ModeChanged`/`ConfigOptions` arms).
    fn refresh_capabilities_for(&self, real_idx: usize) {
        let Some(bridge) = &self.bridge else { return };
        let modes = bridge.session_modes(real_idx);
        self.component
            .set_mode_trigger_label(models::current_mode_name(&modes).into());
        self.component
            .set_mode_dropdown_entries(models::to_mode_dropdown_entries(modes));
        self.component
            .set_config_dropdown_entries(models::to_config_dropdown_entries(
                bridge.config_options(real_idx),
            ));
    }

    /// Rebuilds the `pending-request` property for `real_idx` from the
    /// agent bridge's current pending-request queue -- the request-card
    /// component's whole visibility/content is driven from this, not a
    /// separate boolean flag. Called from [`Self::refresh_messages_for`]
    /// (the shared "this thread became the displayed one" hook, covering
    /// both a genuine thread switch and a same-thread event refresh) so
    /// there's exactly one place that decides which thread's request (if
    /// any) is currently visible.
    fn refresh_pending_request_for(&self, real_idx: usize) {
        let Some(bridge) = &self.bridge else { return };
        let pending = bridge.pending_requests(real_idx);
        let item = match pending.first() {
            Some(event) => {
                let view = permission::to_pending_request_view(event);
                let size = self.component.window().size();
                let scale = self.component.window().scale_factor();
                trace_host_input(format_args!(
                    "pending request active thread={real_idx} method={} window_size={}x{} scale={scale} compact={} narrow={}",
                    event.method,
                    size.width,
                    size.height,
                    self.component.get_compact(),
                    self.component.get_narrow()
                ));
                PendingRequestItem {
                    active: true,
                    relay_id: view.relay_id.into(),
                    method: view.method.into(),
                    title: view.title.into(),
                    summary: view.summary.into(),
                    supported: permission::is_supported_method(&event.method),
                }
            }
            None => PendingRequestItem {
                active: false,
                relay_id: SharedString::default(),
                method: SharedString::default(),
                title: SharedString::default(),
                summary: SharedString::default(),
                supported: false,
            },
        };
        self.component.set_pending_request(item);
    }

    /// Rebuilds the `terminals` row model for `real_idx` from the agent
    /// bridge's current terminal registry (`active_terminals` +
    /// `terminal_buffer`, see `agent_bridge.rs`) and, if the floating
    /// overlay is currently showing one of this thread's terminals,
    /// refreshes `expanded-terminal` too so live output keeps streaming
    /// into an already-open overlay. Called from
    /// [`Self::refresh_messages_for`] (the shared "this thread became
    /// the displayed one" hook) so terminal cards stay in sync with
    /// every event that touches the selected thread, same convention
    /// [`Self::refresh_pending_request_for`] follows.
    fn refresh_terminals_for(&self, real_idx: usize) {
        let Some(bridge) = &self.bridge else { return };
        let ids = bridge.active_terminals(real_idx);
        let entries: Vec<(String, Option<agent_bridge::TerminalBuffer>)> = ids
            .into_iter()
            .map(|id| {
                let buffer = bridge.terminal_buffer(real_idx, &id);
                (id, buffer)
            })
            .collect();
        let expanded_id = self.expanded_terminal_id.borrow().clone();
        if let Some(expanded_id) = &expanded_id {
            if let Some(buffer) = bridge.terminal_buffer(real_idx, expanded_id) {
                self.component.set_expanded_terminal(TerminalItem {
                    terminal_id: expanded_id.clone().into(),
                    output: buffer.output.into(),
                    truncated: buffer.truncated,
                    has_exited: buffer.exit_status.is_some(),
                    exit_code: buffer
                        .exit_status
                        .and_then(|(code, _signal)| code)
                        .unwrap_or_default(),
                });
            }
        }
        self.component
            .set_terminals(models::to_terminal_items(entries));
    }

    /// Rebuilds the `local-terminal` property for `real_idx` from
    /// `AgentBridge::local_terminal_snapshot` -- same "this thread
    /// became the displayed one" hook convention `refresh_terminals_for`
    /// documents, plus called on every periodic poll tick (see
    /// `panel_rust_poll`) regardless of whether any gateway event
    /// arrived, since a client-local PTY's output changes purely from
    /// its own background reader thread, never through `AgentBridge::
    /// poll()`'s event queue at all. Returns whether the rendered
    /// screen text actually changed, so the poll-tick caller only
    /// requests a redraw when there was something new to show (typing
    /// into an idle shell's prompt should not force a redraw every
    /// tick).
    fn refresh_local_terminal_for(&self, real_idx: usize) -> bool {
        let Some(bridge) = &self.bridge else { return false };
        let snapshot = bridge.local_terminal_snapshot(real_idx);
        let new_text = snapshot
            .as_ref()
            .map(|s| s.screen_text.clone())
            .unwrap_or_default();
        let changed = *self.local_terminal_last_text.borrow() != new_text;
        *self.local_terminal_last_text.borrow_mut() = new_text;
        if changed {
            // Coverage-matrix "client PTY" host scenario: a real shell's
            // own screen buffer changing (not a UI flag flip) is the one
            // observable signal that a genuine PTY is running -- traces
            // a tail preview so a host test can confirm it without a
            // screenshot.
            let preview: String = self
                .local_terminal_last_text
                .borrow()
                .chars()
                .rev()
                .take(80)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let preview = preview.replace('\n', "\\n");
            trace_host_input(format_args!(
                "local terminal output thread={real_idx} tail={preview:?}"
            ));
        }
        self.component
            .set_local_terminal(models::to_local_terminal_item(snapshot));
        changed
    }

    /// Answers the currently-displayed thread's first pending request
    /// with `approved`, then immediately re-renders the request card
    /// (which will hide it, since `AgentBridge::respond_to_request`
    /// removes the entry synchronously -- see that method's doc
    /// comment) so the UI reflects the decision without waiting for the
    /// next `apply_bridge_events` poll tick.
    fn answer_pending_request(&self, component: &ChatPanel, approved: bool) {
        let Some(bridge) = &self.bridge else { return };
        let Some(real_idx) = self.real_index(component.get_selected_thread() as usize) else {
            return;
        };
        let pending = bridge.pending_requests(real_idx);
        trace_host_input(format_args!(
            "answer pending request invoked thread={real_idx} approved={approved} pending_count={}",
            pending.len()
        ));
        let Some(event) = pending.first() else { return };
        let response = permission::build_response(event, approved);
        bridge.respond_to_request(real_idx, &event.relay_id, response);
        self.refresh_pending_request_for(real_idx);
    }

    /// Applies queued agent-bridge events to `thread_state` and, if the
    /// currently-selected thread is affected, refreshes the visible
    /// `messages` model too. Returns whether anything visibly changed.
    fn apply_bridge_events(&self) -> bool {
        let Some(bridge) = &self.bridge else {
            return false;
        };
        let events = bridge.poll();
        if events.is_empty() {
            return false;
        }
        // `selected-thread` is a *filtered-list* index (Phase 2) --
        // translate to the real thread index before comparing against
        // `ev.thread_index`, which always refers to the real index the
        // agent bridge knows about.
        let selected = self.real_index(self.component.get_selected_thread() as usize);
        let mut selected_touched = false;
        {
            let mut state = self.thread_state.borrow_mut();
            for ev in &events {
                let idx = ev.thread_index;
                if Some(idx) == selected {
                    selected_touched = true;
                }
                if let Some(slot) = state.get_mut(idx) {
                    match &ev.event {
                        AgentEvent::Message(_) => {} // status unchanged while streaming
                        AgentEvent::TurnEnded(reason) => {
                            trace_host_input(format_args!(
                                "turn ended thread={idx} reason={reason:?}"
                            ));
                            *slot = ThreadState::Idle;
                            if let Some(error) = self.thread_errors.borrow_mut().get_mut(idx) {
                                error.clear();
                            }
                            // queued_send_queue_behavior: auto-advance
                            // this thread's send queue now that its turn
                            // has genuinely ended. `is_compose_focused`
                            // is always passed false here (a simplification
                            // vs. Zed's precedent, which suppresses
                            // auto-send while the user is actively
                            // editing the *next* message) -- this
                            // integration pass doesn't thread per-thread
                            // compose-focus state down to this event
                            // loop; documented, not silently dropped.
                            let popped = self
                                .send_queues
                                .borrow_mut()
                                .get_mut(idx)
                                .and_then(|q| q.on_generation_stopped(false).ok().flatten());
                            if let Some(entry) = popped {
                                bridge.push_local(
                                    idx,
                                    ChatMessage {
                                        kind: MessageKind::User,
                                        text: entry.text.clone(),
                                        status: None,
                                        id: None,
                                        raw_input: None,
                                        raw_output: None,
                                    },
                                );
                                *slot = ThreadState::Loading;
                                bridge.send_prompt(idx, entry.text);
                                trace_host_input(format_args!(
                                    "queued message auto-sent real_thread={idx}"
                                ));
                            }
                        }
                        AgentEvent::Error(error) => {
                            trace_host_input(format_args!(
                                "bridge error thread={idx} error={error:?}"
                            ));
                            *slot = ThreadState::Error;
                            if let Some(slot_error) = self.thread_errors.borrow_mut().get_mut(idx) {
                                *slot_error = error.clone();
                            }
                        }
                        // Rendering itself is driven by re-reading
                        // `AgentBridge::pending_requests(idx)` below (see
                        // `refresh_pending_request_for`), same
                        // "event just signals staleness, state is
                        // re-read from the bridge's own source of
                        // truth" convention `Message` already follows
                        // for history -- this arm only needs to make
                        // sure `selected_touched` covers a request
                        // arriving on the currently-selected thread,
                        // which the loop above already does via `idx`.
                        AgentEvent::PermissionRequest(_) => {}
                        // Same "re-read the bridge's own source of
                        // truth" convention -- `AgentBridge::
                        // terminal_buffer` is what a future terminal
                        // view component would poll; this arm exists
                        // only so the match stays exhaustive and
                        // `selected_touched` covers a terminal thread's
                        // events like every other variant.
                        AgentEvent::TerminalOutput(_) => {}
                        // Same "re-read the bridge's own source of
                        // truth" convention -- `AgentBridge::
                        // session_modes`/`config_options` are what the
                        // settings-sheet mode/config selector polls;
                        // these arms exist only so the match stays
                        // exhaustive and `selected_touched` covers a
                        // capability-advertisement event on the
                        // currently-selected thread like every other
                        // variant.
                        AgentEvent::SessionModes(_)
                        | AgentEvent::CurrentModeChanged(_)
                        | AgentEvent::ConfigOptions(_) => {}
                    }
                }
            }
        }
        self.refresh_threads_model();
        if let (true, Some(selected)) = (selected_touched, selected) {
            self.refresh_messages_for(selected);
        }
        true
    }
}

thread_local! {
    static PANEL: RefCell<Option<PanelSingleton>> = const { RefCell::new(None) };
    // Slint permits one global platform per process. Keep the software
    // window alive across Qt item recreation so a later panel can reuse the
    // already-installed platform instead of calling set_platform again.
    static PLATFORM_WINDOW: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
}

pub struct PanelHandle {
    _private: (),
}

static SENTINEL: PanelHandle = PanelHandle { _private: () };

/// Create (or resize, if already created) the process's single panel
/// instance. See module docs: must only be called from one OS thread, and
/// this process must run with `QSG_RENDER_LOOP=basic`.
#[no_mangle]
pub extern "C" fn panel_rust_create(width: c_uint, height: c_uint) -> *mut PanelHandle {
    PANEL.with(|cell| {
        let mut slot = cell.borrow_mut();
        if let Some(existing) = slot.as_mut() {
            if existing.width != width || existing.height != height {
                existing
                    .window
                    .set_size(slint::PhysicalSize::new(width, height));
                existing.buffer.replace(vec![
                    PremultipliedRgbaColor {
                        red: 0,
                        green: 0,
                        blue: 0,
                        alpha: 0
                    };
                    (width * height) as usize
                ]);
                existing.width = width;
                existing.height = height;
                existing.component.set_compact(width < 320);
                existing.component.set_narrow(width < 220);
                existing.resize_local_terminals_for_viewport();
                existing.window.window().request_redraw();
            }
            return &SENTINEL as *const PanelHandle as *mut PanelHandle;
        }

        let window = PLATFORM_WINDOW.with(|platform_window| {
            let mut platform_window = platform_window.borrow_mut();
            if let Some(window) = platform_window.as_ref() {
                return window.clone();
            }
            let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
            slint::platform::set_platform(Box::new(SpikePlatform {
                window: window.clone(),
            }))
            .expect("panel-rust: set_platform must only be called once per process");
            *platform_window = Some(window.clone());
            window
        });
        window.set_size(slint::PhysicalSize::new(width, height));

        let component = match ChatPanel::new() {
            Ok(c) => c,
            Err(_) => return std::ptr::null_mut(),
        };
        component
            .global::<TextUtil>()
            .on_contains_ci(|haystack, needle| {
                haystack.to_lowercase().contains(&needle.to_lowercase())
            });
        component
            .global::<TextUtil>()
            .on_word_boundary_before(|text, cursor| {
                let text = text.as_str();
                let cursor = (cursor.max(0) as usize).min(text.len());
                if !text.is_char_boundary(cursor) {
                    return cursor as i32;
                }
                let prefix = &text[..cursor];
                let trimmed = prefix.trim_end_matches(char::is_whitespace);
                let start = trimmed
                    .rfind(char::is_whitespace)
                    .map(|i| i + trimmed[i..].chars().next().map_or(1, char::len_utf8))
                    .unwrap_or(0);
                start as i32
            });
        // Compose slash-token helpers -- see `models::active_token_*`.
        component
            .global::<TextUtil>()
            .on_active_token_prefix(|text, cursor| {
                models::active_token_prefix(text.as_str(), cursor).into()
            });
        component
            .global::<TextUtil>()
            .on_active_token_query(|text, cursor| {
                models::active_token_query(text.as_str(), cursor).into()
            });
        component
            .global::<TextUtil>()
            .on_replace_active_token(|text, cursor, replacement| {
                models::replace_active_token(text.as_str(), cursor, replacement.as_str()).into()
            });
        component.set_compact(width < 320);
        component.set_narrow(width < 220);
        window.window().request_redraw();

        // Bridge init failure degrades gracefully rather than aborting
        // panel creation -- the UI still renders (thread list marked
        // "error", compose box becomes a no-op) instead of Shotcut losing
        // the whole dock over a missing/misconfigured agent binary. See
        // `agent_bridge::provision_gateway` and
        // `resolve_acpx_server_bin` determine how each thread's
        // acpx-gateway connection is chosen
        // (RUI_ACPX_<PROVIDER>_URL env override, else a local
        // dev-checkout `acpx-server` auto-spawned against
        // RUI_ACP_AGENT_CMD/the dev-checkout rui-mock-agent path).
        let panel_state = {
            let path = resolve_cache_dir().join("panel-state.sqlite3");
            match PanelStateStore::open(path) {
                Ok(store) => Some(store),
                Err(error) => {
                    eprintln!("panel-rust: panel settings persistence unavailable: {error}");
                    None
                }
            }
        };
        let restored_records = panel_state
            .as_ref()
            .and_then(|store| match store.thread_records() {
                Ok(records) => Some(records),
                Err(error) => {
                    eprintln!("panel-rust: failed to restore chat thread records: {error}");
                    None
                }
            })
            .unwrap_or_default();
        let initial_specs: Vec<ThreadSpec> = if restored_records.is_empty() {
            DEFAULT_THREAD_NAMES
                .iter()
                .enumerate()
                .map(|(idx, name)| ThreadSpec {
                    display_name: (*name).to_owned(),
                    provider: if idx % 2 == 0 { "codex" } else { "claude" }.to_owned(),
                    session_id: None,
                    profile_name: None,
                })
                .collect()
        } else {
            restored_records
                .iter()
                .map(|record| ThreadSpec {
                    display_name: record.display_name.clone(),
                    provider: record.provider.clone(),
                    session_id: Some(record.session_id.clone()),
                    profile_name: record.profile_name.clone(),
                })
                .collect()
        };
        let initial_names: Vec<String> = initial_specs
            .iter()
            .map(|spec| spec.display_name.clone())
            .collect();
        let initial_profiles: Vec<Option<String>> = initial_specs
            .iter()
            .map(|spec| spec.profile_name.clone())
            .collect();
        let initial_permission_profiles: Vec<Option<String>> = restored_records
            .iter()
            .map(|record| record.permission_profile.clone())
            .chain(std::iter::repeat(None))
            .take(initial_names.len())
            .collect();
        let initial_thread_count = initial_names.len();
        let (bridge, initial_state) = match AgentBridge::new_with_thread_specs(&initial_specs) {
            Ok(b) => (Some(b), vec![ThreadState::Idle; initial_names.len()]),
            Err(e) => {
                eprintln!("panel-rust: agent bridge unavailable, chat panel is display-only: {e}");
                (None, vec![ThreadState::Error; initial_names.len()])
            }
        };
        let settings_reload_pending = std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(false),
        );
        let settings_watcher = {
            let pending = settings_reload_pending.clone();
            let paths = settings_file::SettingsPaths::from_env();
            Some(settings_file::SettingsWatcher::start(
                paths,
                std::time::Duration::from_millis(250),
                std::sync::Arc::new(std::sync::Mutex::new(move |_resolved| {
                    pending.store(true, std::sync::atomic::Ordering::SeqCst);
                })),
            ))
        };
        let panel = PanelSingleton {
            window,
            component,
            buffer: RefCell::new(vec![
                PremultipliedRgbaColor {
                    red: 0,
                    green: 0,
                    blue: 0,
                    alpha: 0
                };
                (width * height) as usize
            ]),
            width,
            height,
            bridge,
            panel_state,
            appearance: RefCell::new(AppearanceState::default()),
            active_project_path: RefCell::new(None),
            thread_names: RefCell::new(initial_names),
            thread_profiles: RefCell::new(initial_profiles),
            thread_permission_profiles: RefCell::new(initial_permission_profiles),
            traced_attachment_threads: RefCell::new(HashSet::new()),
            thread_state: RefCell::new(initial_state),
            thread_errors: RefCell::new(Vec::new()),
            send_queues: RefCell::new(
                (0..initial_thread_count)
                    .map(|_| crate::send_queue::SendQueue::new())
                    .collect(),
            ),
            search_query: RefCell::new(String::new()),
            visible_indices: RefCell::new(Vec::new()),
            expanded: RefCell::new(Vec::new()),
            displayed_thread: Cell::new(None),
            expanded_terminal_id: RefCell::new(None),
            local_terminal_last_text: RefCell::new(String::new()),
            settings_reload_pending,
            settings_ignore_watch_until: Cell::new(None),
            _settings_watcher: settings_watcher,
        };
        panel.refresh_threads_model();
        panel.refresh_skills_model();
        // Multi-process prefs live in JSON; selected thread stays in SQLite.
        if let Some(store) = panel.panel_state.as_ref() {
            maybe_migrate_sqlite_defaults_to_json(store);
        }
        let selected_from_sqlite = panel
            .panel_state
            .as_ref()
            .and_then(|store| store.defaults().ok())
            .and_then(|d| d.selected_thread_id);
        let defaults = load_panel_prefs(selected_from_sqlite);
        panel.component.set_default_profile(
            defaults.profile_name.clone().unwrap_or_default().into(),
        );
        panel.component.set_permission_profile(
            defaults
                .permission_profile
                .clone()
                .unwrap_or_default()
                .into(),
        );
        panel
            .component
            .set_background_default(defaults.background_session);
        panel
            .component
            .set_dev_mode(settings_file::SettingsPaths::from_env().dev_mode());
        if let Some(selected_thread_id) = defaults.selected_thread_id {
            if let Some(real_idx) = panel.bridge.as_ref().and_then(|bridge| {
                (0..panel.thread_names.borrow().len()).find(|idx| {
                    bridge
                        .thread_binding(*idx)
                        .is_some_and(|binding| binding.thread_id == selected_thread_id)
                })
            }) {
                if let Some(filtered_idx) = panel
                    .visible_indices
                    .borrow()
                    .iter()
                    .position(|idx| *idx == real_idx)
                {
                    panel.component.set_selected_thread(filtered_idx as i32);
                }
            }
        }
        if let Some(real_idx) = panel.real_index(panel.component.get_selected_thread() as usize) {
            panel.refresh_messages_for(real_idx);
        }

        let component_weak = panel.component.as_weak();
        panel.component.on_thread_selected(move |idx| {
            let Some(_component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    // `idx` is a filtered-list index (Phase 2) -- translate
                    // to the real thread index before touching the bridge.
                    let Some(real_idx) = panel.real_index(idx as usize) else {
                        return;
                    };
                    if let (Some(store), Some(binding)) = (
                        panel.panel_state.as_ref(),
                        panel
                            .bridge
                            .as_ref()
                            .and_then(|bridge| bridge.thread_binding(real_idx)),
                    ) {
                        if let Err(error) =
                            store.set_selected_thread_id(Some(&binding.thread_id))
                        {
                            eprintln!("panel-rust: failed to persist selected chat thread: {error}");
                        }
                    }
                    panel.refresh_messages_for(real_idx);
                    // Settings lists must follow the selected gateway.
                    if panel.component.get_settings_open() {
                        panel.refresh_settings_gateway_lists();
                    }
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_settings_requested(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    // Open immediately so a slow/failed gateway list cannot
                    // leave the user thinking Settings is dead.
                    component.set_settings_open(true);
                    let defaults = load_panel_prefs(None);
                    component.set_default_profile(
                        defaults.profile_name.unwrap_or_default().into(),
                    );
                    component.set_permission_profile(
                        defaults.permission_profile.unwrap_or_default().into(),
                    );
                    component.set_background_default(defaults.background_session);
                    if let Some(store) = panel.panel_state.as_ref() {
                        let selected_override = panel
                            .real_index(component.get_selected_thread() as usize)
                            .and_then(|idx| {
                                panel
                                    .bridge
                                    .as_ref()
                                    .and_then(|bridge| bridge.thread_binding(idx))
                                    .map(|binding| binding.thread_id)
                            })
                            .and_then(|thread_id| {
                                store
                                    .thread_settings(&thread_id)
                                    .ok()
                                    .flatten()
                                    .and_then(|settings| settings.background_session)
                            });
                        component.set_background_override_set(selected_override.is_some());
                        component.set_background_override(selected_override.unwrap_or(false));
                    }
                    panel.refresh_settings_gateway_lists();
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_settings_save(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let defaults = PanelDefaults {
                        profile_name: non_empty(component.get_default_profile().to_string()),
                        permission_profile: non_empty(
                            component.get_permission_profile().to_string(),
                        ),
                        background_session: component.get_background_default(),
                        selected_thread_id: panel
                            .real_index(component.get_selected_thread() as usize)
                            .and_then(|idx| {
                                panel
                                    .bridge
                                    .as_ref()
                                    .and_then(|bridge| bridge.thread_binding(idx))
                                    .map(|binding| binding.thread_id)
                            }),
                    };
                    // JSON is the multi-process source of truth for prefs.
                    if let Err(error) = save_panel_prefs_to_json(&defaults) {
                        eprintln!("panel-rust: failed to save panel settings JSON: {error}");
                        return;
                    }
                    // Mark self-write so the file watcher does not bounce UI.
                    panel.settings_ignore_watch_until.set(Some(
                        std::time::Instant::now() + std::time::Duration::from_millis(500),
                    ));
                    if let Some(store) = panel.panel_state.as_ref() {
                        // Selected thread stays process-local SQLite only.
                        if let Some(thread_id) = defaults.selected_thread_id.as_ref() {
                            if let Err(error) = store.set_selected_thread_id(Some(thread_id)) {
                                eprintln!(
                                    "panel-rust: failed to persist selected chat thread: {error}"
                                );
                            }
                        }
                        if let Some(thread_id) = defaults.selected_thread_id.as_deref() {
                            let override_value = component
                                .get_background_override_set()
                                .then_some(component.get_background_override());
                            if let Err(error) =
                                store.set_background_override(thread_id, override_value)
                            {
                                eprintln!(
                                    "panel-rust: failed to save background-session override: {error}"
                                );
                            }
                        }
                    }
                    panel.refresh_threads_model();
                    component.set_settings_open(false);
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_settings_close(move || {
            if let Some(component) = component_weak.upgrade() {
                component.set_settings_open(false);
            }
        });

        panel.component.on_thread_toggle_background(move |slint_index| {
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(store) = panel.panel_state.as_ref() else {
                        return;
                    };
                    let Some(thread_id) = panel
                        .real_index(slint_index as usize)
                        .and_then(|idx| {
                            panel
                                .bridge
                                .as_ref()
                                .and_then(|bridge| bridge.thread_binding(idx))
                                .map(|binding| binding.thread_id)
                        })
                    else {
                        return;
                    };
                    let next = !store
                        .effective_background_session(&thread_id)
                        .unwrap_or(false);
                    if let Err(error) = store.set_background_override(&thread_id, Some(next)) {
                        eprintln!(
                            "panel-rust: failed to toggle background-session override: {error}"
                        );
                        return;
                    }
                    // Threads model feeds sidebar moon + ChatArea
                    // active-thread-background binding.
                    panel.refresh_threads_model();
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_mcp_server_create(move |name, command| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(bridge) = &panel.bridge else {
                        return;
                    };
                    let entry = if command.is_empty() {
                        serde_json::json!({ "name": name.to_string() })
                    } else {
                        serde_json::json!({ "name": name.to_string(), "command": command.to_string() })
                    };
                    // Don't optimistically append -- re-list from the
                    // gateway's own state either way, same posture as
                    // the mode/config selector's `refresh_capabilities_
                    // for`. A failed create still triggers a re-list so
                    // the sheet reflects reality (e.g. a duplicate name
                    // the gateway rejected).
                    let gw = panel.settings_gateway_index();
                    bridge.create_mcp_server(gw, entry);
                    component.set_available_mcp_servers(models::to_mcp_server_options(
                        bridge.list_mcp_servers(gw),
                    ));
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_mcp_server_delete(move |name| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(bridge) = &panel.bridge else {
                        return;
                    };
                    let gw = panel.settings_gateway_index();
                    bridge.delete_mcp_server(gw, &name);
                    component.set_available_mcp_servers(models::to_mcp_server_options(
                        bridge.list_mcp_servers(gw),
                    ));
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel
            .component
            .on_profile_create(move |name, agent_id, terminal_enabled, fs_enabled| {
                let Some(component) = component_weak.upgrade() else {
                    return;
                };
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        let Some(bridge) = &panel.bridge else {
                            return;
                        };
                        let mut entry = serde_json::json!({
                            "name": name.to_string(),
                            "allow_terminal_access": terminal_enabled,
                            "allow_fs_access": fs_enabled,
                        });
                        if !agent_id.is_empty() {
                            entry["agent_id"] = serde_json::Value::String(agent_id.to_string());
                        }
                        // Don't optimistically append -- re-list from
                        // the gateway's own state either way, same
                        // posture as `on_mcp_server_create` above. A
                        // failed create still triggers a re-list so the
                        // sheet reflects reality (e.g. a duplicate name
                        // the gateway rejected).
                        let gw = panel.settings_gateway_index();
                        bridge.create_profile(gw, entry);
                        component.set_available_profiles(models::to_profile_options(
                            bridge.list_profiles(gw),
                        ));
                    }
                });
            });

        let component_weak = panel.component.as_weak();
        panel.component.on_profile_delete(move |name| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(bridge) = &panel.bridge else {
                        return;
                    };
                    let gw = panel.settings_gateway_index();
                    bridge.delete_profile(gw, &name);
                    component.set_available_profiles(models::to_profile_options(
                        bridge.list_profiles(gw),
                    ));
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_agent_install_requested(move |agent_id| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(bridge) = &panel.bridge else {
                        return;
                    };
                    // Blocking, same posture as `add_thread_with_
                    // profile`'s own gateway calls -- `agents/install`
                    // is a low-frequency settings-sheet action, and
                    // this call can be genuinely slow (a real first-time
                    // npx/binary install). A future progress/job model
                    // is an explicitly open, undecided item (see acpx-
                    // client::ext::registry::install's own doc comment)
                    // -- not addressed by this call site.
                   let gw = panel.settings_gateway_index();
                   bridge.install_agent(gw, &agent_id);
                   component.set_agent_catalog(models::to_agent_catalog_entries(
                       bridge.list_agents(gw),
                   ));
               }
           });
       });

        let component_weak = panel.component.as_weak();
        panel
            .component
            .on_recover_session_attach(move |session_id, provider, title| {
                let Some(component) = component_weak.upgrade() else {
                    return;
                };
                PANEL.with(|cell| {
                    let mut slot = cell.borrow_mut();
                    let Some(panel) = slot.as_mut() else {
                        return;
                    };
                    // Recovery/import (Coverage Matrix `session/list`
                    // row): a brand-new local thread row, bound via
                    // `session/load` to a pre-existing remote session --
                    // explicitly never `session/new`. Name derives from
                    // the backend's own `title` when it has one (real
                    // ACP metadata), falling back to a short, still-
                    // unique session-id-derived label otherwise, same
                    // "always produce a valid slug" posture `on_new_
                    // thread_requested`'s own `format!("New thread
                    // {next_number}")` fallback establishes for its own
                    // case.
                    let title = title.to_string();
                    let base_name = if title.trim().is_empty() {
                        format!(
                            "Recovered {}",
                            session_id.chars().take(8).collect::<String>()
                        )
                    } else {
                        title
                    };
                    let mut name = base_name.clone();
                    let mut suffix = 2;
                    while panel
                        .thread_names
                        .borrow()
                        .iter()
                        .any(|existing| existing == &name)
                    {
                        name = format!("{base_name} ({suffix})");
                        suffix += 1;
                    }
                    let (real_idx, binding, thread_provider) = {
                        let Some(bridge) = panel.bridge.as_mut() else {
                            return;
                        };
                        let Ok(real_idx) = bridge.add_thread_recovering_session(
                            &name,
                            provider.as_str(),
                            session_id.as_str(),
                        ) else {
                            return;
                        };
                        (
                            real_idx,
                            bridge.thread_binding(real_idx),
                            bridge.thread_provider(real_idx),
                        )
                    };
                    if let (Some(store), Some(binding), Some(thread_provider)) = (
                        panel.panel_state.as_ref(),
                        binding.as_ref(),
                        thread_provider.as_ref(),
                    ) {
                        let record = ThreadRecord {
                            thread_id: binding.thread_id.clone(),
                            display_name: name.clone(),
                            provider: thread_provider.clone(),
                            session_id: binding.session_id.clone(),
                            profile_name: None,
                            permission_profile: None,
                            background_session: None,
                        };
                        if let Err(error) = store.save_thread_record(&record) {
                            eprintln!(
                                "panel-rust: failed to persist recovered chat thread: {error}"
                            );
                        }
                    }
                    panel.thread_names.borrow_mut().push(name);
                    panel.thread_profiles.borrow_mut().push(None);
                    panel.thread_permission_profiles.borrow_mut().push(None);
                    panel.thread_state.borrow_mut().push(ThreadState::Idle);
                    panel.thread_errors.borrow_mut().push(String::new());
                    panel
                        .send_queues
                        .borrow_mut()
                        .push(crate::send_queue::SendQueue::new());
                    panel.search_query.borrow_mut().clear();
                    panel.refresh_threads_model();
                    // The recovered session is now bound locally --
                    // refresh the sheet's own list so it no longer
                    // shows as recoverable (matches `recoverable_
                    // sessions`'s own "already bound" exclusion).
                    if let Some(bridge) = panel.bridge.as_ref() {
                        let recovery_provider = bridge.thread_provider(real_idx).unwrap_or_default();
                        component.set_recoverable_sessions(models::to_remote_session_options(
                            bridge.recoverable_sessions(real_idx),
                            &recovery_provider,
                        ));
                    }
                    let filtered_idx = {
                        let visible_indices = panel.visible_indices.borrow();
                        visible_indices.iter().position(|idx| *idx == real_idx)
                    };
                    if let Some(filtered_idx) = filtered_idx {
                        component.set_selected_thread(filtered_idx as i32);
                        if let (Some(store), Some(binding)) = (panel.panel_state.as_ref(), binding)
                        {
                            if let Err(error) =
                                store.set_selected_thread_id(Some(&binding.thread_id))
                            {
                                eprintln!(
                                    "panel-rust: failed to persist selected chat thread: {error}"
                                );
                            }
                        }
                        panel.refresh_messages_for(real_idx);
                    }
                });
            });

        let component_weak = panel.component.as_weak();
        panel.component.on_new_thread_requested(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                let mut slot = cell.borrow_mut();
                let Some(panel) = slot.as_mut() else {
                    return;
                };
                let next_number = panel.thread_names.borrow().len() + 1;
                let name = format!("New thread {next_number}");
                // Profile-picker addition: a new thread opens with
                // whichever profile is currently set as the settings
                // sheet's default (empty means native/unmanaged mode,
                // matching `add_thread`'s prior always-`None` behavior).
                // Prefer resolved JSON prefs (multi-process) then UI field.
                let prefs = load_panel_prefs(None);
                let default_profile = non_empty(component.get_default_profile().to_string())
                    .or(prefs.profile_name);
                let mut profile = default_profile;
                // Resolve gateway index before mutably borrowing the bridge.
                let gw = panel.settings_gateway_index();
                let (real_idx, binding, provider) = {
                    let Some(bridge) = panel.bridge.as_mut() else {
                        return;
                    };
                    // Validate profile name against gateway list when set.
                    if let Some(ref p) = profile {
                        let names: Vec<String> = bridge
                            .list_profiles(gw)
                            .into_iter()
                            .map(|s| s.name)
                            .collect();
                        if !names.is_empty() && !names.iter().any(|n| n == p) {
                            eprintln!(
                                "panel-rust: default profile {p:?} not in gateway list {names:?}; opening unmanaged"
                            );
                            profile = None;
                        }
                    }
                    let Ok(real_idx) = bridge.add_thread_with_profile(&name, profile.as_deref())
                    else {
                        return;
                    };
                    (
                        real_idx,
                        bridge.thread_binding(real_idx),
                        bridge.thread_provider(real_idx),
                    )
                };
                if let (Some(store), Some(binding), Some(provider)) = (
                    panel.panel_state.as_ref(),
                    binding.as_ref(),
                    provider.as_ref(),
                ) {
                    let record = ThreadRecord {
                        thread_id: binding.thread_id.clone(),
                        display_name: name.clone(),
                        provider: provider.clone(),
                        session_id: binding.session_id.clone(),
                        profile_name: profile.clone(),
                        permission_profile: non_empty(
                            component.get_permission_profile().to_string(),
                        ),
                        background_session: None,
                    };
                    if let Err(error) = store.save_thread_record(&record) {
                        eprintln!("panel-rust: failed to persist new chat thread: {error}");
                    }
                }
                panel.thread_names.borrow_mut().push(name);
                panel.thread_profiles.borrow_mut().push(profile);
                panel
                    .thread_permission_profiles
                    .borrow_mut()
                    .push(non_empty(component.get_permission_profile().to_string()));
                panel.thread_state.borrow_mut().push(ThreadState::Idle);
                panel.thread_errors.borrow_mut().push(String::new());
                panel
                    .send_queues
                    .borrow_mut()
                    .push(crate::send_queue::SendQueue::new());
                panel.search_query.borrow_mut().clear();
                // New session: clear compose so it never carries over.
                component.set_compose_text("".into());
                panel.refresh_threads_model();
                let filtered_idx = {
                    let visible_indices = panel.visible_indices.borrow();
                    visible_indices.iter().position(|idx| *idx == real_idx)
                };
                if let Some(filtered_idx) = filtered_idx {
                    component.set_selected_thread(filtered_idx as i32);
                    if let (Some(store), Some(binding)) = (panel.panel_state.as_ref(), binding) {
                        if let Err(error) =
                            store.set_selected_thread_id(Some(&binding.thread_id))
                        {
                            eprintln!("panel-rust: failed to persist selected chat thread: {error}");
                        }
                    }
                    // Fresh empty transcript for the new session row.
                    panel.refresh_messages_for(real_idx);
                }
            });
        });

        panel.component.on_new_skill_requested(move |name| {
            PANEL.with(|cell| {
                let slot = cell.borrow();
                let Some(panel) = slot.as_ref() else {
                    return;
                };
                let dir = crate::skills_state::global_skills_dir(&resolve_cache_dir());
                match crate::skills_state::scaffold_new_skill(&dir, name.as_str()) {
                    Ok(skill_dir) => {
                        trace_host_input(format_args!("new skill scaffolded at {skill_dir:?}"));
                        panel.refresh_skills_model();
                    }
                    Err(error) => {
                        eprintln!("panel-rust: failed to create new skill {name:?}: {error}");
                    }
                }
            });
        });

        panel.component.on_skill_promote_to_global(move |path| {
            PANEL.with(|cell| {
                let slot = cell.borrow();
                let Some(panel) = slot.as_ref() else {
                    return;
                };
                let skill_dir = std::path::PathBuf::from(path.as_str());
                let global_dir = crate::skills_state::global_skills_dir(&resolve_cache_dir());
                match crate::skills_state::promote_skill_to_global(&skill_dir, &global_dir) {
                    Ok(destination) => {
                        trace_host_input(format_args!("skill promoted to global at {destination:?}"));
                        panel.refresh_skills_model();
                    }
                    Err(error) => {
                        eprintln!("panel-rust: failed to promote skill {path:?} to global: {error}");
                    }
                }
            });
        });

        panel.component.on_dev_mode_toggled(move |enabled| {
            let paths = settings_file::SettingsPaths::from_env();
            if let Err(error) = paths.set_dev_mode(enabled) {
                eprintln!("panel-rust: failed to persist dev mode: {error}");
            }
            PANEL.with(|cell| {
                let slot = cell.borrow();
                let Some(panel) = slot.as_ref() else {
                    return;
                };
                panel.component.set_dev_mode(enabled);
                if enabled {
                    let global_dir = crate::skills_state::global_skills_dir(&resolve_cache_dir());
                    if let Err(error) = crate::skills_state::ensure_bundled_global_skill(&global_dir) {
                        eprintln!("panel-rust: failed to install bundled global skill: {error}");
                    }
                    panel.refresh_skills_model();
                }
            });
        });

        panel.component.on_skill_editor_open_requested(move |path| {
            PANEL.with(|cell| {
                let slot = cell.borrow();
                let Some(panel) = slot.as_ref() else {
                    return;
                };
                let path = std::path::PathBuf::from(path.as_str());
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let content =
                    std::fs::read_to_string(path.join("SKILL.md")).unwrap_or_default();
                panel.component.set_active_skill_name(name.into());
                panel
                    .component
                    .set_active_skill_path(path.to_string_lossy().into_owned().into());
                panel.component.set_active_skill_content(content.into());
                let detected: Vec<slint::SharedString> = crate::editor_detect::detect_installed_editors()
                    .into_iter()
                    .map(Into::into)
                    .collect();
                panel
                    .component
                    .set_detected_editors(ModelRc::new(VecModel::from(detected)));
            });
        });

        panel.component.on_skill_content_edited(move |path, content| {
            let skill_md = std::path::Path::new(path.as_str()).join("SKILL.md");
            if let Err(error) = std::fs::write(&skill_md, content.as_str()) {
                eprintln!("panel-rust: failed to save skill {path:?}: {error}");
            }
        });

        panel.component.on_skill_copy_path_requested(move |path| {
            trace_host_input(format_args!("skill copy-path requested for {path:?}"));
            // No system clipboard dependency in this crate today -- see
            // panel-rust/Cargo.lock check in skill-manager-workspace's
            // 03-open-risks.md for the same "no new dependency without a
            // concrete need" stance applied to the opener crate. Logged
            // for now; a real clipboard write is a small, separate
            // addition once a clipboard crate is actually needed
            // elsewhere too.
        });

        panel.component.on_skill_open_in_editor_requested(move |editor_name, path| {
            let Some((bin, _)) = crate::editor_detect::EDITOR_CANDIDATES
                .iter()
                .find(|(_, name)| *name == editor_name.as_str())
            else {
                eprintln!("panel-rust: unknown editor {editor_name:?}");
                return;
            };
            if let Err(error) =
                crate::editor_detect::open_in_editor(bin, std::path::Path::new(path.as_str()))
            {
                eprintln!("panel-rust: failed to open skill in {editor_name:?}: {error}");
            }
        });

        panel.component.on_skill_open_with_os_default_requested(move |path| {
            if let Err(error) =
                crate::editor_detect::open_with_os_default(std::path::Path::new(path.as_str()))
            {
                eprintln!("panel-rust: failed to open skill with OS default: {error}");
            }
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_send_requested(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            let text = component.get_compose_text().to_string();
            let text = text.trim();
            if text.is_empty() {
                trace_host_input("send requested with empty composer");
                return;
            }
            let filtered_idx = component.get_selected_thread() as usize;
            trace_host_input(format_args!(
                "send requested selected_thread={filtered_idx} text={text:?}"
            ));
            component.set_compose_text("".into());
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(idx) = panel.real_index(filtered_idx) else {
                        return;
                    };
                    let Some(bridge) = &panel.bridge else { return };
                    if panel
                        .thread_state
                        .borrow()
                        .get(idx)
                        .is_some_and(|state| *state == ThreadState::Loading)
                    {
                        // queued_send_queue_behavior: a turn is already
                        // in flight, so this message goes on the queue
                        // instead of being silently dropped (compose is
                        // no longer disabled while sending -- see
                        // chat_input_layout.slint's `enabled: true` --
                        // so this branch is reachable for real now,
                        // unlike before this phase).
                        if let Some(queue) = panel.send_queues.borrow_mut().get_mut(idx) {
                            match queue.enqueue(text.to_string(), false) {
                                Ok(_) => trace_host_input(format_args!(
                                    "send queued real_thread={idx} (turn in flight)"
                                )),
                                Err(error) => eprintln!(
                                    "panel-rust: failed to enqueue message for thread {idx}: {error}"
                                ),
                            }
                        }
                        return;
                    }
                    if bridge.thread_closed(idx) {
                        trace_host_input(format_args!(
                            "send ignored real_thread={idx} because the thread is closed"
                        ));
                        return;
                    }
                    if let Some(error) = panel.thread_errors.borrow_mut().get_mut(idx) {
                        error.clear();
                    }
                    bridge.push_local(
                        idx,
                        ChatMessage {
                            kind: MessageKind::User,
                            text: text.to_string(),
                            status: None,
                            id: None,
                            raw_input: None,
                            raw_output: None,
                        },
                    );
                    if let Some(slot) = panel.thread_state.borrow_mut().get_mut(idx) {
                        *slot = ThreadState::Loading;
                    }
                    panel.refresh_threads_model();
                    if Some(idx) == panel.real_index(component.get_selected_thread() as usize) {
                        panel.refresh_messages_for(idx);
                    }
                    bridge.send_prompt(idx, text.to_string());
                    trace_host_input(format_args!("send dispatched real_thread={idx}"));
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_stop_requested(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(idx) = panel.real_index(component.get_selected_thread() as usize)
                    else {
                        return;
                    };
                    if !matches!(
                        panel.thread_state.borrow().get(idx),
                        Some(ThreadState::Loading)
                    ) {
                        return;
                    }
                    if let Some(slot) = panel.thread_state.borrow_mut().get_mut(idx) {
                        *slot = ThreadState::Cancelling;
                    }
                    panel.refresh_threads_model();
                    panel
                        .bridge
                        .as_ref()
                        .map(|bridge| bridge.cancel_prompt(idx));
                }
            });
        });

        // Coverage Matrix `session/close`/`session/delete` row --
        // explicit, opt-in-only thread lifecycle controls, gated by the
        // sidebar row's own two-step arm/confirm UI. `filtered_idx` is
        // the Slint-side (possibly search-filtered) row index, same
        // "translate through `real_index` before touching the bridge"
        // convention every other sidebar-row callback here uses.
        let component_weak = panel.component.as_weak();
        panel.component.on_thread_close_requested(move |filtered_idx| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(idx) = panel.real_index(filtered_idx as usize) else {
                        return;
                    };
                    let Some(bridge) = &panel.bridge else { return };
                    if !bridge.close_thread(idx) {
                        return;
                    }
                    // Stop treating a closed session as in-flight.
                    if let Some(slot) = panel.thread_state.borrow_mut().get_mut(idx) {
                        if *slot == ThreadState::Loading || *slot == ThreadState::Cancelling {
                            *slot = ThreadState::Idle;
                        }
                    }
                    panel.refresh_threads_model();
                    // If the closed row is still selected, re-project history
                    // (read-only) so the UI reflects closed without a blank
                    // reload race; send path already rejects closed threads.
                    if panel.real_index(component.get_selected_thread() as usize) == Some(idx) {
                        panel.refresh_messages_for(idx);
                    }
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_thread_delete_requested(move |filtered_idx| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(idx) = panel.real_index(filtered_idx as usize) else {
                        return;
                    };
                    let Some(bridge) = &panel.bridge else { return };
                    if !bridge.delete_thread(idx) {
                        return;
                    }
                    if let Some(slot) = panel.thread_state.borrow_mut().get_mut(idx) {
                        *slot = ThreadState::Idle;
                    }
                    panel.refresh_threads_model();
                    if panel.real_index(component.get_selected_thread() as usize) == Some(idx) {
                        panel.refresh_messages_for(idx);
                    }
                }
            });
        });

        // Interactive agent-request relay addition: approve/reject
        // buttons on the request card built by `refresh_pending_request_
        // for`. Both handlers re-read the exact `AgentRequestEvent` from
        // `AgentBridge::pending_requests` (rather than trusting only the
        // Slint-side `PendingRequestItem` snapshot's `relay-id` string)
        // so `permission::build_response` gets the real, untruncated
        // `raw_request` needed to build a native `session/request_
        // permission` reply -- the Slint struct only carries a
        // human-readable summary, not the full JSON.
        let component_weak = panel.component.as_weak();
        panel.component.on_approve_request(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    panel.answer_pending_request(&component, true);
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_reject_request(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    panel.answer_pending_request(&component, false);
                }
            });
        });

        // Terminal-view addition: expand a card into the floating
        // overlay, and close it. `refresh_terminals_for` (called from
        // every `refresh_messages_for`) keeps whichever terminal is
        // currently expanded live-updating; these two callbacks only
        // own which id (if any) is expanded.
        let component_weak = panel.component.as_weak();
        panel.component.on_expand_terminal(move |terminal_id| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    *panel.expanded_terminal_id.borrow_mut() = Some(terminal_id.to_string());
                    let Some(real_idx) = panel.real_index(component.get_selected_thread() as usize)
                    else {
                        return;
                    };
                    panel.refresh_terminals_for(real_idx);
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_close_terminal_overlay(move || {
            let Some(_component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    *panel.expanded_terminal_id.borrow_mut() = None;
                }
            });
        });

        // Client-local PTY terminal addition -- toggle open/closed,
        // forward keyboard input, and an explicit kill action. Real
        // `LocalTerminal::spawn`/`close_local_terminal`, no simulation
        // -- see `local_terminal.rs`'s doc comment.
        let component_weak = panel.component.as_weak();
        panel.component.on_local_terminal_toggle_requested(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            trace_host_input("local terminal toggle callback invoked");
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(bridge) = &panel.bridge else { return };
                    let Some(real_idx) = panel.real_index(component.get_selected_thread() as usize)
                    else {
                        return;
                    };
                    if bridge.has_local_terminal(real_idx) {
                        bridge.close_local_terminal(real_idx);
                        trace_host_input(format_args!(
                            "local terminal toggled thread={real_idx} open=false"
                        ));
                    } else {
                        let (cols, rows) = panel.local_terminal_dimensions();
                        bridge.open_local_terminal(real_idx, cols, rows);
                        trace_host_input(format_args!(
                            "local terminal toggled thread={real_idx} open=true cols={cols} rows={rows}"
                        ));
                    }
                    panel.refresh_local_terminal_for(real_idx);
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_local_terminal_key_input(move |text| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(bridge) = &panel.bridge else { return };
                    let Some(real_idx) = panel.real_index(component.get_selected_thread() as usize)
                    else {
                        return;
                    };
                    let bytes = models::translate_local_terminal_key(text.as_str());
                    if !bytes.is_empty() {
                        bridge.write_local_terminal_input(real_idx, &bytes);
                        trace_host_input(format_args!(
                            "local terminal key thread={real_idx} bytes={:?}",
                            String::from_utf8_lossy(&bytes)
                        ));
                    }
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_local_terminal_close_requested(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(bridge) = &panel.bridge else { return };
                    let Some(real_idx) = panel.real_index(component.get_selected_thread() as usize)
                    else {
                        return;
                    };
                    bridge.close_local_terminal(real_idx);
                    panel.refresh_local_terminal_for(real_idx);
                }
            });
        });

        // Mode/config selector addition: dispatch `session/set_mode`/
        // `session/set_config_option` on the *currently displayed*
        // thread. Neither callback optimistically updates `current-
        // mode-id`/`config-option-rows` itself -- both wait for the
        // real backend's own confirmation (`AgentEvent::
        // CurrentModeChanged`/`ConfigOptions`, applied by `apply_bridge_
        // events` -> `refresh_capabilities_for`), matching `AgentBridge::
        // set_mode`/`set_config_option`'s own "requested, not applied"
        // doc comment -- a backend can reject/ignore the request or
        // resolve to a different value than requested (config options
        // especially: changing one can change others), and this UI
        // should never show a selection the backend didn't actually
        // confirm.
        let component_weak = panel.component.as_weak();
        panel.component.on_mode_selected(move |mode_id| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(bridge) = &panel.bridge else { return };
                    let Some(real_idx) =
                        panel.real_index(component.get_selected_thread() as usize)
                    else {
                        return;
                    };
                    bridge.set_mode(real_idx, mode_id.to_string());
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel
            .component
            .on_config_option_selected(move |option_id, value| {
                let Some(component) = component_weak.upgrade() else {
                    return;
                };
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        let Some(bridge) = &panel.bridge else { return };
                        let Some(real_idx) =
                            panel.real_index(component.get_selected_thread() as usize)
                        else {
                            return;
                        };
                        bridge.set_config_option(
                            real_idx,
                            option_id.to_string(),
                            serde_json::Value::String(value.to_string()),
                        );
                    }
                });
            });

        let component_weak = panel.component.as_weak();
        panel.component.on_search_changed(move |query| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    *panel.search_query.borrow_mut() = query.to_string();
                    panel.refresh_threads_model();
                    // The filter can move/remove the previously-selected
                    // row entirely -- reset to the first still-visible
                    // thread (Phase 2 UX decision, documented in the
                    // theme-parity plan's Phase 2 section) rather than
                    // leaving a stale/out-of-range selection.
                    component.set_selected_thread(0);
                    match panel.real_index(0) {
                        Some(real_idx) => panel.refresh_messages_for(real_idx),
                        None => component
                            .set_messages(ModelRc::new(VecModel::from(Vec::<MessageItem>::new()))),
                    }
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_toggle_expanded(move |index| {
            let Some(_component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(real_idx) = panel.displayed_thread.get() else {
                        return;
                    };
                    let idx = index as usize;
                    let mut expanded = panel.expanded.borrow_mut();
                    if let Some(slot) = expanded.get_mut(idx) {
                        *slot = !*slot;
                    }
                    drop(expanded);
                    panel.render_messages(real_idx);
                }
            });
        });

        // Phase 3 step 2: invoked by the message Flickable's real top-edge
        // gesture or its accessible fallback action. Slint raises the
        // loading guard before this callback, so reset it on every outcome.
        let component_weak = panel.component.as_weak();
        panel.component.on_load_older_requested(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    let Some(bridge) = &panel.bridge else { return };
                    let Some(real_idx) = panel.displayed_thread.get() else {
                        return;
                    };
                    let before_len = bridge.transcript(real_idx).len();
                    if bridge.load_older_page(real_idx) {
                        let after_len = bridge.transcript(real_idx).len();
                        // The new rows were prepended at the *front* --
                        // grow `expanded` from the front too, so every
                        // pre-existing collapse-state entry stays
                        // attached to the same logical message it
                        // described before this reload, not silently
                        // shifted onto whatever now occupies its old
                        // index.
                        let grown_by = after_len.saturating_sub(before_len);
                        if grown_by > 0 {
                            let mut expanded = panel.expanded.borrow_mut();
                            let mut prefixed = vec![false; grown_by];
                            prefixed.append(&mut expanded);
                            *expanded = prefixed;
                        }
                        panel.render_messages(real_idx);
                    }
                }
            });
            component.set_loading_older_messages(false);
        });

        *slot = Some(panel);
        &SENTINEL as *const PanelHandle as *mut PanelHandle
    })
}

#[no_mangle]
pub extern "C" fn panel_rust_destroy(_handle: *mut PanelHandle) {
    // The C ABI handle is a process-local sentinel; the actual ownership is
    // the thread-local singleton. Clearing it drops AgentBridge and stops
    // local actors when Qt destroys or recreates the dock.
    PANEL.with(|cell| {
        cell.borrow_mut().take();
    });
}

/// Forward a click at physical pixel coordinates, as a press+release pair.
#[no_mangle]
pub extern "C" fn panel_rust_input_click(_handle: *mut PanelHandle, x: c_uint, y: c_uint) -> bool {
    let window = PANEL.with(|cell| cell.borrow().as_ref().map(|panel| panel.window.clone()));
    let Some(window) = window else {
        return false;
    };
    let pos = slint::LogicalPosition::new(x as f32, y as f32);
    let win = window.window();
    win.dispatch_event(WindowEvent::PointerMoved { position: pos });
    win.dispatch_event(WindowEvent::PointerPressed {
        position: pos,
        button: PointerEventButton::Left,
    });
    win.dispatch_event(WindowEvent::PointerReleased {
        position: pos,
        button: PointerEventButton::Left,
    });
    let (compose_has_focus, selected_thread, selected_state) = PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return (false, -1, String::from("no-panel"));
        };
        let selected_thread = panel.component.get_selected_thread();
        let selected_state = panel
            .real_index(selected_thread.max(0) as usize)
            .and_then(|idx| panel.thread_state.borrow().get(idx).cloned())
            .map(|state| format!("{state:?}"))
            .unwrap_or_else(|| String::from("no-thread"));
        (
            panel.component.get_compose_has_focus(),
            selected_thread,
            selected_state,
        )
    });
    trace_host_input(format_args!(
        "click x={x} y={y} compose_focus={compose_has_focus} selected_thread={selected_thread} state={selected_state}"
    ));
    true
}

/// Forwards a Qt wheel/touchpad gesture in logical pixels. Slint's nested
/// Flickables consume only the scroll they can apply and automatically bubble
/// any boundary remainder to their parent surface.
#[no_mangle]
pub extern "C" fn panel_rust_input_scroll(
    _handle: *mut PanelHandle,
    x: f32,
    y: f32,
    delta_x: f32,
    delta_y: f32,
) -> bool {
    let window = PANEL.with(|cell| cell.borrow().as_ref().map(|panel| panel.window.clone()));
    let Some(window) = window else {
        return false;
    };
    window.window().dispatch_event(WindowEvent::PointerScrolled {
        position: slint::LogicalPosition::new(x, y),
        delta_x,
        delta_y,
    });
    true
}

/// Forward a keyboard event -- `qt_key` is `QKeyEvent::key()`, `text` is
/// `QKeyEvent::text()` UTF-8 encoded (may be empty for pure modifiers).
/// See `map_qt_key` for the Qt -> Slint key mapping. Needed for the chat
/// compose box (`TextInput` in the markup above); clicking into it via
/// `panel_rust_input_click` already gives it focus the same way any Slint
/// app would.
#[no_mangle]
pub extern "C" fn panel_rust_input_key(
    _handle: *mut PanelHandle,
    qt_key: c_int,
    text_ptr: *const c_uchar,
    text_len: usize,
    pressed: bool,
) -> bool {
    let window = PANEL.with(|cell| cell.borrow().as_ref().map(|panel| panel.window.clone()));
    let Some(window) = window else {
        return false;
    };
    let text = if text_ptr.is_null() || text_len == 0 {
        ""
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(text_ptr, text_len) };
        std::str::from_utf8(bytes).unwrap_or("")
    };
    // The host must not consume editor shortcuts unless an editable Slint
    // surface owns focus. Besides the composer, a local PTY terminal is a
    // genuine keyboard target and must receive printable keys, editing keys,
    // and arrows without Shotcut handling them as global shortcuts.
    let (compose_has_focus, local_terminal_has_focus) = PANEL.with(|cell| {
        cell.borrow().as_ref().map_or((false, false), |panel| {
            (
                panel.component.get_compose_has_focus(),
                panel.component.get_local_terminal_has_focus(),
            )
        })
    });
    if !compose_has_focus && !local_terminal_has_focus {
        trace_host_input(format_args!(
            "key qt_key={qt_key:#x} pressed={pressed} text={text:?} \
             compose_focus=false local_terminal_focus=false"
        ));
        return false;
    }
    // TextInput consumes text on key press. Forwarding Qt's matching release
    // with the same text can make a character appear twice in an embedded
    // host, so consume releases after the focus guard without redispatching
    // their text to Slint.
    if !pressed {
        trace_host_input(format_args!(
            "key qt_key={qt_key:#x} pressed=false text={text:?} \
             compose_focus={compose_has_focus} local_terminal_focus={local_terminal_has_focus}"
        ));
        return true;
    }
    let Some(key_text) = map_qt_key(qt_key, text) else {
        trace_host_input(format_args!(
            "key qt_key={qt_key:#x} pressed=true text={text:?} \
             compose_focus={compose_has_focus} local_terminal_focus={local_terminal_has_focus} \
             mapped=false"
        ));
        return false;
    };
    trace_host_input(format_args!(
        "key qt_key={qt_key:#x} pressed=true text={text:?} \
         compose_focus={compose_has_focus} local_terminal_focus={local_terminal_has_focus} \
         mapped={key_text:?}"
    ));
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: key_text });
    true
}

/// Sets the theme variant ("dark"/"light"/anything else treated as dark),
/// per `MainWindow::changeTheme()`'s resolved theme name -- see
/// `ChatRustDock::applyTheme` on the C++ side. Returns whether the panel
/// exists to apply it to.
#[no_mangle]
pub extern "C" fn panel_rust_set_theme(
    _handle: *mut PanelHandle,
    text_ptr: *const c_uchar,
    text_len: usize,
) -> bool {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        let text = if text_ptr.is_null() || text_len == 0 {
            "dark"
        } else {
            let bytes = unsafe { std::slice::from_raw_parts(text_ptr, text_len) };
            std::str::from_utf8(bytes).unwrap_or("dark")
        };
        Theme::get(&panel.component).set_theme(text.into());
        true
    })
}

/// `active_project_binding` phase's FFI crossing point -- mirrors
/// `panel_rust_set_theme`'s byte-buffer shape exactly.
/// `ChatRustDock::updateProjectPath` calls this whenever `MainWindow::
/// producerOpened` fires, passing `MainWindow::fileName()`. An empty
/// buffer (zero length, not necessarily a null pointer) means "no
/// project open" and clears the stored path -- Shotcut's own
/// `producerOpened(false)` firing on project close is expected to pass
/// an empty string, not skip the call, so panel-rust's state can't go
/// stale after a close.
#[no_mangle]
pub extern "C" fn panel_rust_set_project_path(
    _handle: *mut PanelHandle,
    path_ptr: *const c_uchar,
    path_len: usize,
) -> bool {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        let path = if path_ptr.is_null() || path_len == 0 {
            None
        } else {
            let bytes = unsafe { std::slice::from_raw_parts(path_ptr, path_len) };
            std::str::from_utf8(bytes).ok().map(str::to_string)
        };
        // `chat_sessions_project_path` phase: also propagate to the
        // bridge, whose `cwd_for_session` reads this to scope every
        // subsequently-opened ACP session to the active project instead
        // of the process's own working directory.
        if let Some(bridge) = panel.bridge.as_ref() {
            bridge.set_active_project_path(path.clone().map(std::path::PathBuf::from));
        }
        panel
            .component
            .set_active_project_path(path.clone().unwrap_or_default().into());
        *panel.active_project_path.borrow_mut() = path;
        // `project_scoped_skill_isolation`: re-scan now that the active
        // project (and therefore its `.skills/` directory) changed.
        panel.refresh_skills_model();
        true
    })
}

/// Applies a generation-ordered host appearance snapshot. The host owns only
/// selector values; the panel retains its component palette and tokens.
#[no_mangle]
pub extern "C" fn panel_rust_apply_appearance(
    _handle: *mut PanelHandle,
    generation: u64,
    dark: bool,
) -> bool {
    panel_rust_apply_host_appearance(
        _handle,
        generation,
        dark,
        std::ptr::null(),
        0,
        std::ptr::null(),
        0,
        1.0,
        1.0,
    )
}

/// Applies a full, generation-ordered host appearance snapshot. UTF-8
/// strings are copied before they reach Slint, so Qt-owned buffers never
/// outlive this call.
#[no_mangle]
pub extern "C" fn panel_rust_apply_host_appearance(
    _handle: *mut PanelHandle,
    generation: u64,
    dark: bool,
    language_ptr: *const c_uchar,
    language_len: usize,
    font_ptr: *const c_uchar,
    font_len: usize,
    font_scale: f32,
    density: f32,
) -> bool {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        let decode_utf8 = |ptr: *const c_uchar, len: usize| {
            if ptr.is_null() || len == 0 {
                String::new()
            } else {
                let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
                std::str::from_utf8(bytes).unwrap_or("").to_owned()
            }
        };
        let font_scale = font_scale.clamp(0.5, 3.0);
        let density = density.clamp(0.5, 4.0);
        let appearance = HostAppearance {
            generation,
            color_scheme: if dark {
                ColorScheme::Dark
            } else {
                ColorScheme::Light
            },
            language_tag: decode_utf8(language_ptr, language_len),
            bundled_font: decode_utf8(font_ptr, font_len),
            font_scale,
            density,
        };
        if !panel.appearance.borrow_mut().apply(appearance) {
            return false;
        }
        let appearance_state = panel.appearance.borrow();
        let appearance = appearance_state
            .current()
            .expect("appearance was applied above");
        let theme = Theme::get(&panel.component);
        theme.set_theme(if dark { "dark" } else { "light" }.into());
        theme.set_host_language_tag(appearance.language_tag.clone().into());
        theme.set_host_font_sans(appearance.bundled_font.clone().into());
        theme.set_host_font_scale(appearance.font_scale);
        theme.set_host_density(appearance.density);
        panel
            .window
            .window()
            .dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: appearance.density,
            });
        panel.window.window().request_redraw();
        true
    })
}

/// Drains any pending agent-bridge events (streamed message chunks,
/// turn-end, errors) and applies them to the Slint model. Must be called
/// periodically from the C++ side (a `QTimer`, see `ChatRustDock`) since
/// nothing else drives the single-threaded Slint/Qt loop to notice
/// background agent activity -- see `agent_bridge` module docs. Returns
/// whether anything changed (caller should then call
/// `panel_rust_render` + trigger a Qt repaint).
#[no_mangle]
pub extern "C" fn panel_rust_poll(_handle: *mut PanelHandle) -> bool {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        let bridge_changed = panel.apply_bridge_events();
        panel.sync_thread_records();
        // Multi-process settings watch: reload prefs when another process
        // rewrote the global/project JSON (skip during our own save window).
        let mut settings_changed = false;
        if panel
            .settings_reload_pending
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            let ignore = panel
                .settings_ignore_watch_until
                .get()
                .is_some_and(|until| std::time::Instant::now() < until);
            if !ignore && panel.component.get_settings_open() {
                // v1 dirty policy: always refresh when settings open
                // (operator multi-process sync wins over half-edited form).
                panel.apply_json_prefs_to_component();
                settings_changed = true;
            } else if !ignore {
                panel.apply_json_prefs_to_component();
                settings_changed = true;
            }
        }
        // Client-local PTY terminal output arrives on its own
        // background reader thread, never through `AgentBridge::
        // poll()`'s event queue -- refresh it unconditionally on every
        // tick (not gated behind `apply_bridge_events`'s own "any
        // gateway events at all" early return), independent of whether
        // any gateway activity happened this tick.
        let selected = panel.real_index(panel.component.get_selected_thread() as usize);
        let local_terminal_changed = selected
            .map(|idx| panel.refresh_local_terminal_for(idx))
            .unwrap_or(false);
        let connection_changed = selected
            .map(|idx| panel.refresh_connection_status_for(idx))
            .unwrap_or(false);
        bridge_changed || local_terminal_changed || connection_changed || settings_changed
    })
}

#[no_mangle]
pub extern "C" fn panel_rust_render(_handle: *mut PanelHandle) -> bool {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        let width = panel.width;
        panel.window.draw_if_needed(|renderer| {
            let mut buffer = panel.buffer.borrow_mut();
            renderer.render(&mut buffer, width as usize);
        })
    })
}

#[no_mangle]
pub extern "C" fn panel_rust_buffer_ptr(_handle: *mut PanelHandle) -> *const c_uchar {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        match slot.as_ref() {
            Some(panel) => panel.buffer.borrow().as_ptr() as *const c_uchar,
            None => std::ptr::null(),
        }
    })
}

#[no_mangle]
pub extern "C" fn panel_rust_buffer_len(_handle: *mut PanelHandle) -> usize {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        match slot.as_ref() {
            Some(panel) => {
                panel.buffer.borrow().len() * std::mem::size_of::<PremultipliedRgbaColor>()
            }
            None => 0,
        }
    })
}

#[no_mangle]
pub extern "C" fn panel_rust_width(_handle: *mut PanelHandle) -> c_uint {
    PANEL.with(|cell| cell.borrow().as_ref().map(|p| p.width).unwrap_or(0))
}

#[no_mangle]
pub extern "C" fn panel_rust_height(_handle: *mut PanelHandle) -> c_uint {
    PANEL.with(|cell| cell.borrow().as_ref().map(|p| p.height).unwrap_or(0))
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn panel_create_destroy_create_reuses_slint_platform() {
        // Force the bridge into its documented display-only fallback so
        // this lifecycle regression test does not depend on a developer's
        // running gateways or mutate an external session.
        let cache_dir = tempfile::tempdir().expect("cache dir");
        let previous = [
            (
                "RUI_ACPX_CODEX_URL",
                std::env::var("RUI_ACPX_CODEX_URL").ok(),
            ),
            (
                "RUI_ACPX_CLAUDE_URL",
                std::env::var("RUI_ACPX_CLAUDE_URL").ok(),
            ),
            ("RUI_ACP_CACHE_DIR", std::env::var("RUI_ACP_CACHE_DIR").ok()),
        ];
        std::env::set_var("RUI_ACPX_CODEX_URL", "http://127.0.0.1:1");
        std::env::set_var("RUI_ACPX_CLAUDE_URL", "http://127.0.0.1:1");
        std::env::set_var("RUI_ACP_CACHE_DIR", cache_dir.path());

        let first = panel_rust_create(96, 64);
        assert!(!first.is_null());
        assert_eq!(panel_rust_width(first), 96);
        assert_eq!(panel_rust_height(first), 64);
        assert!(panel_rust_render(first));
        assert!(panel_rust_input_scroll(first, 48.0, 32.0, 0.0, -40.0));
        PANEL.with(|cell| {
            let panel = cell.borrow();
            let panel = panel.as_ref().expect("panel exists");
            panel.component.set_compose_text("preserve this draft".into());
        });
        assert!(panel_rust_apply_host_appearance(
            first,
            1,
            false,
            b"fr-FR".as_ptr(),
            b"fr-FR".len(),
            b"Noto Sans".as_ptr(),
            b"Noto Sans".len(),
            1.25,
            2.0,
        ));
        PANEL.with(|cell| {
            let panel = cell.borrow();
            let panel = panel.as_ref().expect("panel exists");
            let appearance = panel.appearance.borrow();
            assert_eq!(appearance.current().unwrap().language_tag, "fr-FR");
            assert_eq!(appearance.current().unwrap().bundled_font, "Noto Sans");
            assert_eq!(panel.component.get_compose_text(), "preserve this draft");
            let theme = Theme::get(&panel.component);
            assert_eq!(theme.get_theme(), "light");
            assert_eq!(theme.get_host_language_tag(), "fr-FR");
            assert_eq!(theme.get_host_font_sans(), "Noto Sans");
            assert_eq!(theme.get_host_font_scale(), 1.25);
            assert_eq!(theme.get_host_density(), 2.0);
        });
        assert!(!panel_rust_apply_host_appearance(
            first,
            1,
            true,
            b"en-US".as_ptr(),
            b"en-US".len(),
            b"Different".as_ptr(),
            b"Different".len(),
            1.0,
            1.0,
        ));
        panel_rust_destroy(first);
        assert_eq!(panel_rust_width(first), 0);

        let second = panel_rust_create(128, 72);
        assert!(!second.is_null());
        assert_eq!(panel_rust_width(second), 128);
        assert_eq!(panel_rust_height(second), 72);
        assert!(panel_rust_render(second));
        panel_rust_destroy(second);
        assert_eq!(panel_rust_width(second), 0);

        for (key, value) in previous {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
    }
}
