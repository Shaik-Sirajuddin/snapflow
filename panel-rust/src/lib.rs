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
mod dirty;
mod dispatch;
mod editor_detect;
mod effect;
mod effect_executor;
mod external_snapshot;
pub mod gateway_actor;
pub mod jsonl_store;
mod list_model;
mod local_terminal;
mod markdown;
mod model;
pub mod models;
mod msg;
mod permission;
pub mod protocol_types;
mod send_queue;
mod settings_file;
// `pub` (not just `mod`) so the new `skills-mcp-server` bin target can
// reuse `scan_skills_dir`/`global_skills_dir`/`project_skills_dir` instead
// of duplicating the SKILL.md front-matter parsing logic.
pub mod skills_state;
mod state_store;
mod sync;
mod theme;
mod update;

use agent_bridge::{resolve_cache_dir, AgentBridge, ThreadSpec};
use appearance::{ColorScheme, HostAppearance};
use models::ThreadState;
use protocol_types::{ChatMessage, MessageKind};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Key, Platform, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{SharedString, VecModel};
use state_store::{PanelDefaults, PanelStateStore};
use std::cell::{Cell, RefCell};
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
/// Maps a bare `Qt::Key_Shift/Control/Meta/Alt` code to Slint's matching
/// `Key`. Shared between `map_qt_key`'s press-side special-case table and
/// `panel_rust_input_key`'s release handling -- see the doc comments at
/// both call sites for why bare modifier keys need both press *and*
/// release forwarded, unlike every other key this bridge handles.
fn modifier_key_for_qt_key(qt_key: c_int) -> Option<Key> {
    match qt_key {
        0x0100_0020 => Some(Key::Shift),
        0x0100_0021 => Some(Key::Control),
        0x0100_0022 => Some(Key::Meta),
        0x0100_0023 => Some(Key::Alt),
        _ => None,
    }
}

fn map_qt_key(qt_key: c_int, text: &str, shift: bool) -> Option<SharedString> {
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
        // Bare modifier presses. Without these, a real Ctrl/Alt press is
        // never forwarded as a `KeyPressed` at all (their `QKeyEvent::
        // text()` is empty and their `qt_key` is far outside the 0x20-0x7E
        // ASCII-graphic range the `text.is_empty()` fallback below
        // recovers), so Slint's own internal modifier tracking
        // (`InternalKeyboardModifierState`, keyed off exactly these
        // `Key::*` text values) never learns a modifier is held -- every
        // `event.modifiers.control`/`.alt`/`.shift` check anywhere in the
        // UI (Ctrl+B/N/K/Alt+Up/Down here, Shift+Enter in the compose box,
        // etc.) would silently always read `false` when driven by a real
        // host keyboard, despite working in slint-viewer/tests that
        // dispatch these `Key::*` events directly.
        _ => modifier_key_for_qt_key(qt_key),
    };
    if let Some(k) = special {
        return Some(k.into());
    }
    // Ctrl+<letter> delivers `QKeyEvent::text()` as the classic ASCII
    // control character (Ctrl+A=0x01 .. Ctrl+Z=0x1A), not the letter
    // itself -- confirmed live: a real Ctrl+B combo arrives with
    // text="\u{2}". Every Ctrl+<letter> shortcut check in this UI
    // (`handle-panel-shortcut`'s Ctrl+B/N/, branches, TextInput's own
    // built-in Ctrl+A select-all) compares against the literal letter
    // with `event.modifiers.control` already true (real modifier
    // tracking -- see the bare-modifier-press mapping above), so those
    // checks silently never matched through the real host bridge. Recover
    // the letter here once, centrally, instead of needing every call site
    // to know about raw control-character text. Case is unrecoverable
    // from the control byte alone (Ctrl+B and Ctrl+Shift+B produce the
    // same 0x02) -- lowercase is fine since every affected call site
    // already checks both cases. Only single-char control-range text is
    // affected; the qt_key codes already handled above (Escape/Tab/
    // Backspace/Return/Delete/Home/End/arrows/bare modifiers) never reach
    // this point, so a genuine Tab/Return press can't be misread as
    // Ctrl+I/Ctrl+M here.
    if let Some(ch) = text.chars().next() {
        if text.chars().count() == 1 && ('\u{1}'..='\u{1a}').contains(&ch) {
            let letter = (b'a' + (ch as u8 - 1)) as char;
            return Some(letter.into());
        }
    }
    if text.is_empty() {
        // QQuickItem receives an empty `QKeyEvent::text()` for some printable
        // keys when the host also owns a shortcut for that key. Qt still
        // provides the ASCII `Qt::Key_*` code, so recover that character for
        // a focused composer instead of letting host shortcuts eat the input.
        // Shifted/non-ASCII input continues to use Qt's non-empty text path.
        //
        // `Qt::Key_A`..`Key_Z` are case-*insensitive* constants (always
        // 0x41-0x5A/uppercase, regardless of whether Shift was actually
        // held -- Qt only conveys case via `text()`, never via `key()`).
        // Every other printable `Qt::Key_*` in the 0x20-0x7E range (digits,
        // punctuation) already resolves to the shift-corrected character on
        // X11 (a keysym's shift level is baked into which keysym the
        // keycode maps to), so only the letter-case decision below needs
        // the caller's own `shift` (real modifier state, passed through
        // from `QKeyEvent::modifiers()`) rather than being guessable from
        // `qt_key` alone -- unconditionally lower-casing here (the
        // previous behavior) silently dropped every actually-uppercase
        // letter typed while it collided with a host shortcut.
        match u32::try_from(qt_key)
            .ok()
            .and_then(char::from_u32)
            .filter(|ch| ch.is_ascii_graphic() || *ch == ' ')
        {
            Some(ch) if ch.is_ascii_uppercase() && !shift => Some(ch.to_ascii_lowercase().into()),
            Some(ch) => Some(ch.into()),
            None => None,
        }
    } else {
        Some(SharedString::from(text))
    }
}

#[cfg(test)]
mod map_qt_key_tests {
    use super::map_qt_key;
    use slint::platform::Key;
    use slint::SharedString;
    use std::os::raw::c_int;

    const QT_KEY_A: c_int = 0x41;
    const QT_KEY_B: c_int = 0x42;
    const QT_KEY_Z: c_int = 0x5a;
    const QT_KEY_TAB: c_int = 0x0100_0001;
    const QT_KEY_RETURN: c_int = 0x0100_0004;

    #[test]
    fn ctrl_letter_control_characters_recover_the_plain_letter() {
        // Real Ctrl+A/B/Z combos deliver the classic ASCII control byte as
        // QKeyEvent::text(), not the letter -- confirmed live.
        assert_eq!(map_qt_key(QT_KEY_A, "\u{1}", false).unwrap(), "a");
        assert_eq!(map_qt_key(QT_KEY_B, "\u{2}", false).unwrap(), "b");
        assert_eq!(map_qt_key(QT_KEY_Z, "\u{1a}", false).unwrap(), "z");
    }

    #[test]
    fn ctrl_shift_letter_still_recovers_the_letter_despite_lost_case() {
        // Ctrl+B and Ctrl+Shift+B both produce the same 0x02 byte -- case
        // is genuinely unrecoverable from the control character alone.
        assert_eq!(map_qt_key(QT_KEY_B, "\u{2}", true).unwrap(), "b");
    }

    #[test]
    fn plain_tab_and_return_are_unaffected() {
        // Tab (0x09) and Return (0x0d) fall in the same 0x01..=0x1a
        // control-character range as Ctrl+I/Ctrl+M, but a real Tab/Return
        // press arrives via their own dedicated qt_key special case
        // (checked first), never reaching the Ctrl+<letter> recovery path.
        assert_eq!(
            map_qt_key(QT_KEY_TAB, "\t", false).unwrap(),
            SharedString::from(Key::Tab)
        );
        assert_eq!(
            map_qt_key(QT_KEY_RETURN, "\r", false).unwrap(),
            SharedString::from(Key::Return)
        );
    }

    #[test]
    fn regular_printable_text_passes_through_unchanged() {
        assert_eq!(map_qt_key(QT_KEY_A, "a", false).unwrap(), "a");
        assert_eq!(map_qt_key(QT_KEY_B, "B", false).unwrap(), "B");
    }

    #[test]
    fn multi_char_text_is_not_treated_as_a_control_character() {
        assert_eq!(map_qt_key(QT_KEY_A, "ab", false).unwrap(), "ab");
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
        Ok(resolved) => settings_file::resolved_to_panel_defaults(&resolved, selected_thread_id),
        Err(error) => {
            eprintln!("panel-rust: settings file load failed: {error}");
            PanelDefaults {
                selected_thread_id,
                ..PanelDefaults::default()
            }
        }
    }
}

/// Settings values displayed for one editable tier. The Project view reads
/// Project → Global → bundled defaults, while Global reads Global → bundled
/// defaults. Saving the view writes only the selected tier's document.
struct ScopedPanelPrefs {
    defaults: PanelDefaults,
    default_agent_id: Option<String>,
}

fn scoped_settings_path<'a>(
    paths: &'a settings_file::SettingsPaths,
    scope: &str,
) -> Option<&'a std::path::Path> {
    match scope {
        "global" => Some(paths.global.as_path()),
        "project" => paths.project.as_deref(),
        _ => None,
    }
}

fn load_scoped_panel_prefs(
    scope: &str,
    selected_thread_id: Option<String>,
) -> Option<ScopedPanelPrefs> {
    let paths = settings_file::SettingsPaths::from_env();
    if scoped_settings_path(&paths, scope).is_none() {
        eprintln!("panel-rust: unavailable settings scope {scope:?}");
        return None;
    }

    let mut documents = Vec::new();
    if let Some(path) = paths.bundled_default.as_deref() {
        match settings_file::load_document(path) {
            Ok(document) => documents.push(document),
            Err(error) => {
                eprintln!("panel-rust: bundled settings load failed: {error}");
                return None;
            }
        }
    }
    match settings_file::load_document(&paths.global) {
        Ok(document) => documents.push(document),
        Err(error) => {
            eprintln!("panel-rust: global settings load failed: {error}");
            return None;
        }
    }
    if scope == "project" {
        let Some(path) = paths.project.as_deref() else {
            return None;
        };
        match settings_file::load_document(path) {
            Ok(document) => documents.push(document),
            Err(error) => {
                eprintln!("panel-rust: project settings load failed: {error}");
                return None;
            }
        }
    }

    let refs: Vec<&settings_file::SettingsDocument> = documents.iter().collect();
    let resolved = settings_file::merge_documents(&refs);
    Some(ScopedPanelPrefs {
        defaults: settings_file::resolved_to_panel_defaults(&resolved, selected_thread_id),
        default_agent_id: resolved.default_agent_id,
    })
}

/// Persist profile / permission / background-default / default-agent into the
/// selected JSON tier. Existing unrelated fields (harness, dev mode, ...) are
/// retained by the read-modify-write operation.
fn save_panel_prefs_to_json(
    scope: &str,
    defaults: &PanelDefaults,
    default_agent_id: Option<String>,
) -> Result<(), String> {
    let paths = settings_file::SettingsPaths::from_env();
    let path = scoped_settings_path(&paths, scope)
        .ok_or_else(|| format!("settings scope {scope:?} is unavailable"))?;
    let mut doc = settings_file::load_document(path).map_err(|error| error.to_string())?;
    doc.schema_version = 1;
    doc.default_profile = defaults.profile_name.clone();
    doc.permission_profile = defaults.permission_profile.clone();
    doc.background_session_default = Some(defaults.background_session);
    doc.default_agent_id = default_agent_id;
    settings_file::save_document(path, &doc).map_err(|error| error.to_string())
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
    /// Persistent TEA model. Dispatchers must fold every message into this
    /// instance; constructing a stand-in model per callback loses state
    /// between events and makes stale-result handling impossible.
    model: RefCell<model::Model>,
    buffer: RefCell<Vec<PremultipliedRgbaColor>>,
    width: u32,
    height: u32,
    bridge: Option<AgentBridge>,
    panel_state: Option<PanelStateStore>,
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
        let selected_thread = self.model.borrow().selected_thread;
        if let Some(idx) = self.real_index(selected_thread) {
            if self
                .bridge
                .as_ref()
                .and_then(|b| b.thread_binding(idx))
                .is_some()
            {
                return idx;
            }
        }
        let n = self.model.borrow().threads.len();
        if let Some(bridge) = self.bridge.as_ref() {
            for idx in 0..n {
                if bridge.thread_binding(idx).is_some() {
                    return idx;
                }
            }
        }
        0
    }

    pub(crate) fn dispatch_frame_input(&self, frame: msg::FrameInput) -> bool {
        crate::dispatch::dispatch_frame_input(self, frame)
    }

    fn sync_runtime_defaults(&self, effective: &PanelDefaults) {
        let Some(store) = self.panel_state.as_ref() else {
            return;
        };
        let selected_thread_id = store
            .defaults()
            .ok()
            .and_then(|defaults| defaults.selected_thread_id);
        let runtime_defaults = PanelDefaults {
            profile_name: effective.profile_name.clone(),
            permission_profile: effective.permission_profile.clone(),
            background_session: effective.background_session,
            selected_thread_id,
        };
        if let Err(error) = store.save_defaults(&runtime_defaults) {
            eprintln!("panel-rust: failed to synchronize runtime panel defaults: {error}");
        }
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
        for idx in 0..self.model.borrow().threads.len() {
            if bridge.has_local_terminal(idx) {
                bridge.resize_local_terminal(idx, cols, rows);
            }
        }
    }

    /// Translates a Slint-side filtered-list index (what `thread-selected`
    /// callbacks and `get_selected_thread()` hand back) into the real
    /// index the agent bridge/`thread_state` use. `None` if out of range
    /// (e.g. the filter just emptied the list out from under a stale
    /// selection).
    fn real_index(&self, filtered_idx: usize) -> Option<usize> {
        self.model
            .borrow()
            .visible_indices
            .get(filtered_idx)
            .copied()
    }

    /// `dispatch.rs`'s Compose-domain wrapper (tea-slint-model Phase 4)
    /// calls this -- extracted verbatim from the former
    /// `on_send_requested` closure body, see that module's doc comment
    /// for why the real bridge/queue-aware cascade stays here rather
    /// than being reimplemented against `Model`.
    pub(crate) fn execute_send_prompt_real(&self, real_idx: usize, text: &str) {
        let Some(bridge) = &self.bridge else { return };
        self.start_send_prompt(real_idx, text, bridge);
    }

    fn start_send_prompt(&self, idx: usize, text: &str, bridge: &AgentBridge) {
        if bridge.thread_closed(idx) {
            trace_host_input(format_args!(
                "send ignored real_thread={idx} because the thread is closed"
            ));
            return;
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
        bridge.send_prompt(idx, text.to_string());
        trace_host_input(format_args!("send dispatched real_thread={idx}"));
    }

    /// Executes the bridge side of the cancellation effect. State
    /// transitions are owned by `update()`.
    pub(crate) fn execute_cancel_generation_real(&self, real_idx: usize) {
        self.bridge
            .as_ref()
            .map(|bridge| bridge.cancel_prompt(real_idx));
    }

    /// Answers the currently-displayed thread's first pending request
    /// with a concrete one-of option id (Zed flat permission model), then
    /// immediately re-renders the request card (which will hide it, since
    /// `AgentBridge::respond_to_request` removes the entry synchronously).
    /// `dispatch.rs`'s Request-domain wrapper (tea-slint-model Phase 4)
    /// calls this -- extracted verbatim from the former
    /// `on_load_older_requested` closure's `PANEL.with` body.
    pub(crate) fn dispatch_load_older_requested(&self) {
        let Some(bridge) = &self.bridge else { return };
        let Some(real_idx) = self.model.borrow().displayed_thread else {
            return;
        };
        let before_len = bridge.transcript(real_idx).len();
        if bridge.load_older_page(real_idx) {
            let after_len = bridge.transcript(real_idx).len();
            // The new rows were prepended at the *front* -- grow
            // `expanded` from the front too, so every pre-existing
            // collapse-state entry stays attached to the same logical
            // message it described before this reload, not silently
            // shifted onto whatever now occupies its old index.
            let grown_by = after_len.saturating_sub(before_len);
            self.dispatch_frame_input(msg::FrameInput {
                prepend_expanded_rows: grown_by,
                selected_thread_snapshot: crate::external_snapshot::ExternalSnapshotSource::new(
                    self,
                )
                .collect_thread_snapshot_for(real_idx),
                ..msg::FrameInput::default()
            });
        }
    }

    /// Extracted from the former `on_local_terminal_toggle_requested`
    /// closure body.
    pub(crate) fn dispatch_local_terminal_toggle(&self) {
        trace_host_input("local terminal toggle callback invoked");
        let Some(bridge) = &self.bridge else { return };
        let Some(real_idx) = self.real_index(self.model.borrow().selected_thread) else {
            return;
        };
        if bridge.has_local_terminal(real_idx) {
            bridge.close_local_terminal(real_idx);
            trace_host_input(format_args!(
                "local terminal toggled thread={real_idx} open=false"
            ));
        } else {
            let (cols, rows) = self.local_terminal_dimensions();
            bridge.open_local_terminal(real_idx, cols, rows);
            trace_host_input(format_args!(
                "local terminal toggled thread={real_idx} open=true cols={cols} rows={rows}"
            ));
        }
    }

    /// Extracted from the former `on_local_terminal_key_input` closure body.
    pub(crate) fn dispatch_local_terminal_key_input(&self, text: &str) {
        let Some(bridge) = &self.bridge else { return };
        let Some(real_idx) = self.real_index(self.model.borrow().selected_thread) else {
            return;
        };
        let bytes = models::translate_local_terminal_key(text);
        if !bytes.is_empty() {
            bridge.write_local_terminal_input(real_idx, &bytes);
            trace_host_input(format_args!(
                "local terminal key thread={real_idx} bytes={:?}",
                String::from_utf8_lossy(&bytes)
            ));
        }
    }

    /// Extracted from the former `on_local_terminal_close_requested`
    /// closure body.
    pub(crate) fn dispatch_local_terminal_close(&self) {
        let Some(bridge) = &self.bridge else { return };
        let Some(real_idx) = self.real_index(self.model.borrow().selected_thread) else {
            return;
        };
        bridge.close_local_terminal(real_idx);
    }

    /// `dispatch.rs`'s Settings-domain wrappers (tea-slint-model Phase 4)
    /// call these -- extracted verbatim from the former `on_settings_*`/
    /// `on_mcp_server_*`/`on_profile_*`/`on_mode_selected`/
    /// `on_config_option_selected`/`on_agent_install_requested`/
    /// `on_dev_mode_toggled` closure bodies.
    pub(crate) fn dispatch_settings_requested(&self) {
        self.dispatch_frame_input(msg::FrameInput {
            settings_preferences_snapshot: Some(
                crate::external_snapshot::ExternalSnapshotSource::new(self)
                    .collect_settings_preferences_snapshot(None),
            ),
            ..msg::FrameInput::default()
        });
        self.dispatch_frame_input(msg::FrameInput {
            settings_gateway_snapshot: Some(
                crate::external_snapshot::ExternalSnapshotSource::new(self)
                    .collect_settings_gateway_snapshot(),
            ),
            ..msg::FrameInput::default()
        });
    }

    pub(crate) fn dispatch_settings_scope_changed(&self, scope: &str) {
        self.dispatch_frame_input(msg::FrameInput {
            settings_preferences_snapshot: Some(
                crate::external_snapshot::ExternalSnapshotSource::new(self)
                    .collect_settings_preferences_snapshot(Some(scope)),
            ),
            ..msg::FrameInput::default()
        });
    }

    pub(crate) fn execute_settings_save(
        &self,
        input: msg::SettingsSaveInput,
    ) -> Result<(), effect::EffectError> {
        let defaults = PanelDefaults {
            profile_name: non_empty(input.default_profile),
            permission_profile: non_empty(input.permission_profile),
            background_session: input.background_default,
            selected_thread_id: input.selected_thread_id.clone(),
        };
        if let Err(error) = save_panel_prefs_to_json(
            input.scope.as_str(),
            &defaults,
            non_empty(input.default_agent_id),
        ) {
            return Err(effect::EffectError::new(format!(
                "failed to save panel settings JSON: {error}"
            )));
        }
        self.sync_runtime_defaults(&load_panel_prefs(None));
        self.settings_ignore_watch_until.set(Some(
            std::time::Instant::now() + std::time::Duration::from_millis(500),
        ));
        if let Some(store) = self.panel_state.as_ref() {
            if let Some(thread_id) = defaults.selected_thread_id.as_ref() {
                if let Err(error) = store.set_selected_thread_id(Some(thread_id)) {
                    return Err(effect::EffectError::new(format!(
                        "failed to persist selected chat thread: {error}"
                    )));
                }
            }
            if let Some(thread_id) = defaults.selected_thread_id.as_deref() {
                let override_value = input
                    .background_override_set
                    .then_some(input.background_override);
                if let Err(error) = store.set_background_override(thread_id, override_value) {
                    return Err(effect::EffectError::new(format!(
                        "failed to save background-session override: {error}"
                    )));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn dispatch_mcp_server_create(
        &self,
        _component: &ChatPanel,
        name: &str,
        command: &str,
    ) {
        let Some(bridge) = &self.bridge else { return };
        let entry = if command.is_empty() {
            serde_json::json!({ "name": name })
        } else {
            serde_json::json!({ "name": name, "command": command })
        };
        let gw = self.settings_gateway_index();
        bridge.create_mcp_server(gw, entry);
    }

    pub(crate) fn dispatch_mcp_server_delete(&self, _component: &ChatPanel, name: &str) {
        let Some(bridge) = &self.bridge else { return };
        let gw = self.settings_gateway_index();
        bridge.delete_mcp_server(gw, name);
    }

    pub(crate) fn dispatch_mcp_server_enabled_changed(
        &self,
        _component: &ChatPanel,
        name: &str,
        enabled: bool,
    ) {
        let Some(bridge) = &self.bridge else { return };
        let gw = self.settings_gateway_index();
        let Some(mut entry) = bridge
            .list_mcp_servers(gw)
            .into_iter()
            .find(|entry| entry.name == name)
        else {
            eprintln!(
                "panel-rust: MCP server {:?} disappeared before its enabled state could update",
                name
            );
            return;
        };
        entry.extra["enabled"] = serde_json::Value::Bool(enabled);
        if !bridge.update_mcp_server(gw, entry.extra) {
            eprintln!(
                "panel-rust: failed to update enabled state for MCP server {:?}",
                name
            );
        }
    }

    pub(crate) fn dispatch_profile_create(
        &self,
        _component: &ChatPanel,
        name: &str,
        agent_id: Option<&str>,
        terminal_enabled: bool,
        fs_enabled: bool,
    ) {
        let Some(bridge) = &self.bridge else { return };
        let mut entry = serde_json::json!({
            "name": name,
            "allow_terminal_access": terminal_enabled,
            "allow_fs_access": fs_enabled,
        });
        if let Some(agent_id) = agent_id.filter(|s| !s.is_empty()) {
            entry["agent_id"] = serde_json::Value::String(agent_id.to_string());
        }
        let gw = self.settings_gateway_index();
        bridge.create_profile(gw, entry);
    }

    pub(crate) fn dispatch_profile_delete(&self, _component: &ChatPanel, name: &str) {
        let Some(bridge) = &self.bridge else { return };
        let gw = self.settings_gateway_index();
        bridge.delete_profile(gw, name);
    }

    pub(crate) fn dispatch_agent_install_requested(&self, _component: &ChatPanel, agent_id: &str) {
        let Some(bridge) = &self.bridge else { return };
        let gw = self.settings_gateway_index();
        bridge.install_agent(gw, agent_id);
    }

    pub(crate) fn dispatch_dev_mode_toggled(&self, enabled: bool) {
        let paths = settings_file::SettingsPaths::from_env();
        if let Err(error) = paths.set_dev_mode(enabled) {
            eprintln!("panel-rust: failed to persist dev mode: {error}");
        }
        if enabled {
            let global_dir = crate::skills_state::global_skills_dir(&resolve_cache_dir());
            if let Err(error) = crate::skills_state::ensure_bundled_global_skill(&global_dir) {
                eprintln!("panel-rust: failed to install bundled global skill: {error}");
            }
        }
    }

    pub(crate) fn dispatch_mode_selected(&self, mode_id: &str) {
        let Some(bridge) = &self.bridge else { return };
        let Some(real_idx) = self.real_index(self.model.borrow().selected_thread) else {
            return;
        };
        bridge.set_mode(real_idx, mode_id.to_string());
    }

    pub(crate) fn dispatch_config_option_selected(&self, option_id: &str, value: &str) {
        let Some(bridge) = &self.bridge else { return };
        let Some(real_idx) = self.real_index(self.model.borrow().selected_thread) else {
            return;
        };
        bridge.set_config_option(
            real_idx,
            option_id.to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }

    /// `dispatch.rs`'s Skill-domain wrappers (tea-slint-model Phase 4)
    /// call these -- extracted verbatim from the former `on_new_skill_
    /// requested`/`on_skill_*` closure bodies.
    pub(crate) fn dispatch_new_skill_requested(&self, name: &str, scope: &str) {
        let skill_scope = match scope {
            "global" => crate::skills_state::SkillScope::Global,
            "project" => crate::skills_state::SkillScope::Project,
            other => {
                eprintln!("panel-rust: invalid new skill scope {other:?}");
                return;
            }
        };
        let active_project_path = self.model.borrow().active_project_path.clone();
        let active_project_file = active_project_path.as_deref().map(std::path::Path::new);
        let dir = match crate::skills_state::skill_creation_dir(
            skill_scope,
            &resolve_cache_dir(),
            active_project_file,
        ) {
            Ok(dir) => dir,
            Err(error) => {
                eprintln!(
                    "panel-rust: failed to resolve {scope} skill storage for {name:?}: {error}"
                );
                return;
            }
        };
        match crate::skills_state::scaffold_new_skill(&dir, name) {
            Ok(skill_dir) => {
                trace_host_input(format_args!("new skill scaffolded at {skill_dir:?}"));
                self.dispatch_frame_input(msg::FrameInput {
                    skills_snapshot: Some(
                        crate::external_snapshot::ExternalSnapshotSource::new(self)
                            .collect_skills_snapshot(),
                    ),
                    ..msg::FrameInput::default()
                });
                crate::dispatch::dispatch_skill_editor_open_requested(
                    self,
                    skill_dir.to_string_lossy().into_owned(),
                );
            }
            Err(error) => {
                eprintln!("panel-rust: failed to create new skill {name:?}: {error}");
            }
        }
    }

    pub(crate) fn dispatch_skill_copy_path_requested(&self, path: &str) {
        trace_host_input(format_args!("skill copy-path requested for {path:?}"));
    }

    pub(crate) fn dispatch_skill_open_in_editor_requested(&self, editor_name: &str, path: &str) {
        let Some((bin, _)) = crate::editor_detect::EDITOR_CANDIDATES
            .iter()
            .find(|(_, name)| *name == editor_name)
        else {
            eprintln!("panel-rust: unknown editor {editor_name:?}");
            return;
        };
        if let Err(error) = crate::editor_detect::open_in_editor(bin, std::path::Path::new(path)) {
            eprintln!("panel-rust: failed to open skill in {editor_name:?}: {error}");
        }
    }

    pub(crate) fn dispatch_skill_open_with_os_default_requested(&self, path: &str) {
        if let Err(error) = crate::editor_detect::open_with_os_default(std::path::Path::new(path)) {
            eprintln!("panel-rust: failed to open skill with OS default: {error}");
        }
    }

    pub(crate) fn open_skill_search_result(&self, query: &str, show_global: bool) {
        let needle = query.trim().to_lowercase();
        let global_dir = crate::skills_state::global_skills_dir(&resolve_cache_dir());
        let mut entries = if show_global {
            crate::skills_state::scan_skills_dir(
                &global_dir,
                crate::skills_state::SkillScope::Global,
            )
        } else {
            Vec::new()
        };
        let active_project_path = self.model.borrow().active_project_path.clone();
        if let Some(project_path) = active_project_path.as_ref() {
            if let Some(project_dir) = std::path::Path::new(project_path).parent() {
                entries.extend(crate::skills_state::scan_skills_dir(
                    &crate::skills_state::project_skills_dir(project_dir),
                    crate::skills_state::SkillScope::Project,
                ));
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        if let Some(entry) = entries.into_iter().find(|entry| {
            needle.is_empty()
                || entry.name.to_lowercase().contains(&needle)
                || entry.description.to_lowercase().contains(&needle)
        }) {
            crate::dispatch::dispatch_skill_editor_open_requested(
                self,
                entry.path.to_string_lossy().into_owned(),
            );
        }
    }

    /// Executes the bridge-side half of `Effect::SetActiveProjectPath`.
    /// This must stay separate from `dispatch_project_path_changed`, which
    /// creates that effect; calling the dispatcher here would recurse forever.
    pub(crate) fn apply_active_project_path(&self, path: Option<String>) {
        if let Some(bridge) = self.bridge.as_ref() {
            bridge.set_active_project_path(path.clone().map(std::path::PathBuf::from));
        }
    }

    // `dispatch.rs`'s Request-domain wrappers (tea-slint-model Phase 4)
    // call this directly.
    pub(crate) fn answer_pending_request_option(&self, option_id: &str) {
        let Some(bridge) = &self.bridge else { return };
        let Some(real_idx) = self.real_index(self.model.borrow().selected_thread) else {
            return;
        };
        let pending = bridge.pending_requests(real_idx);
        trace_host_input(format_args!(
            "answer pending request option thread={real_idx} option_id={option_id} pending_count={}",
            pending.len()
        ));
        let Some(event) = pending.first() else { return };
        let response = permission::build_response_for_option(event, option_id);
        bridge.respond_to_request(real_idx, &event.relay_id, response);
        self.dispatch_frame_input(msg::FrameInput {
            selected_thread_snapshot: crate::external_snapshot::ExternalSnapshotSource::new(self)
                .collect_thread_snapshot_for(real_idx),
            ..msg::FrameInput::default()
        });
    }

    /// Keyboard convenience: approve/reject maps to the first allow_* /
    /// reject_* option on the live request (same fallback as
    /// [`permission::build_response`]).
    // `dispatch.rs`'s Request-domain wrappers (tea-slint-model Phase 4)
    // call this directly.
    pub(crate) fn answer_pending_request(&self, approved: bool) {
        let Some(bridge) = &self.bridge else { return };
        let Some(real_idx) = self.real_index(self.model.borrow().selected_thread) else {
            return;
        };
        let pending = bridge.pending_requests(real_idx);
        let Some(event) = pending.first() else { return };
        let options = permission::extract_options(event);
        let option_id = if approved {
            permission::default_allow_option_id(&options)
        } else {
            permission::default_reject_option_id(&options)
        };
        if let Some(id) = option_id {
            self.answer_pending_request_option(id);
            return;
        }
        // No matching option (e.g. reject with only allow offered) —
        // fall through to build_response's cancel policy.
        let response = permission::build_response(event, approved);
        bridge.respond_to_request(real_idx, &event.relay_id, response);
        self.dispatch_frame_input(msg::FrameInput {
            selected_thread_snapshot: crate::external_snapshot::ExternalSnapshotSource::new(self)
                .collect_thread_snapshot_for(real_idx),
            ..msg::FrameInput::default()
        });
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
                // `resize` in place, not `replace(vec![...])` -- a live
                // window/dock drag fires this on every intermediate
                // geometry step, and the previous full
                // fresh-allocate-and-zero-every-pixel approach did real,
                // avoidable work on every single one of those ticks (a
                // full heap allocation plus writing every element to the
                // same default color, discarding the old buffer's already-
                // reserved capacity every time). `resize` reuses existing
                // capacity when the new size fits (the common case for
                // small drag deltas) and only initializes newly-added
                // elements when growing -- correctness is unaffected by
                // stale/reinterpreted content from a width change, since
                // panel_rust_render always redraws every pixel of the
                // buffer fresh on the next frame regardless (this is a
                // full-buffer software renderer, not incremental), so
                // nothing here is ever visible before that overwrite.
                // Reported symptom this closes: "resize is not smooth,
                // layout elements bump up and down a bit" -- the
                // reallocation cost on every drag tick could fall behind
                // Qt's own frame pacing, visibly desyncing the panel's
                // content from the window chrome resizing around it.
                existing.buffer.borrow_mut().resize(
                    (width * height) as usize,
                    PremultipliedRgbaColor {
                        red: 0,
                        green: 0,
                        blue: 0,
                        alpha: 0,
                    },
                );
                existing.width = width;
                existing.height = height;
                crate::sync::sync_geometry(&existing.component, width < 320, width < 220);
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
        crate::sync::sync_geometry(&component, width < 320, width < 220);
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
        let initial_permission_profiles: Vec<Option<String>> = restored_records
            .iter()
            .map(|record| record.permission_profile.clone())
            .chain(std::iter::repeat(None))
            .take(initial_specs.len())
            .collect();
        let (bridge, bridge_available) = match AgentBridge::new_with_thread_specs(&initial_specs) {
            Ok(b) => (Some(b), true),
            Err(e) => {
                eprintln!("panel-rust: agent bridge unavailable, chat panel is display-only: {e}");
                (None, false)
            }
        };
        let initial_selected_thread_id = panel_state
            .as_ref()
            .and_then(|store| store.defaults().ok())
            .and_then(|defaults| defaults.selected_thread_id);
        let settings_reload_pending =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
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
        let mut model = model::Model::from_initial_state(model::InitialState {
            threads: initial_specs.clone(),
            thread_ids: restored_records
                .iter()
                .map(|record| record.thread_id.clone())
                .chain(
                    (restored_records.len()..initial_specs.len())
                        .map(|idx| format!("thread:{idx}")),
                )
                .collect(),
            selected_thread_id: initial_selected_thread_id.clone(),
            permission_profiles: initial_permission_profiles.clone(),
            thread_states: if bridge_available {
                vec![ThreadState::Idle; initial_specs.len()]
            } else {
                vec![ThreadState::Error; initial_specs.len()]
            },
        });
        let thread_model = Rc::new(VecModel::default());
        let messages_model = Rc::new(VecModel::default());
        let skills_model = Rc::new(VecModel::default());
        model.thread_model = thread_model.clone();
        model.messages_model = messages_model.clone();
        model.skills_model = skills_model.clone();
        let panel = PanelSingleton {
            window,
            component,
            model: RefCell::new(model),
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
            settings_reload_pending,
            settings_ignore_watch_until: Cell::new(None),
            _settings_watcher: settings_watcher,
        };
        crate::sync::sync_initial_models(&panel.model.borrow(), &panel.component);
        // Cold start enters through the same TEA path as every later event.
        // The store/bridge snapshot has already been collected above, so
        // this synchronous effect completion is the local executor for the
        // initial-state load. It establishes the first Model before any
        // Slint callback can observe the panel.
        {
            let initial = model::InitialState {
                threads: initial_specs.clone(),
                thread_ids: restored_records
                    .iter()
                    .map(|record| record.thread_id.clone())
                    .chain(
                        (restored_records.len()..initial_specs.len())
                            .map(|idx| format!("thread:{idx}")),
                    )
                    .collect(),
                selected_thread_id: initial_selected_thread_id.clone(),
                permission_profiles: initial_permission_profiles.clone(),
                thread_states: if bridge_available {
                    vec![ThreadState::Idle; initial_specs.len()]
                } else {
                    vec![ThreadState::Error; initial_specs.len()]
                },
            };
            dispatch::dispatch_initial_hydration(&panel, initial);
        }
        // Fold the first bridge/store-backed presentation snapshot as one
        // Frame message. This makes cold start's first post-hydration sync a
        // single reducer turn instead of several adapter-driven pushes.
        panel.dispatch_frame_input(crate::msg::FrameInput {
            thread_list_snapshot: Some(
                crate::external_snapshot::ExternalSnapshotSource::new(&panel)
                    .collect_thread_list_snapshot(),
            ),
            skills_snapshot: Some(
                crate::external_snapshot::ExternalSnapshotSource::new(&panel)
                    .collect_skills_snapshot(),
            ),
            ..crate::msg::FrameInput::default()
        });
        // Multi-process prefs live in JSON; selected thread stays in SQLite.
        if let Some(store) = panel.panel_state.as_ref() {
            maybe_migrate_sqlite_defaults_to_json(store);
        }
        let selected_from_sqlite = panel
            .panel_state
            .as_ref()
            .and_then(|store| store.defaults().ok())
            .and_then(|d| d.selected_thread_id);
        let settings_scope = if settings_file::SettingsPaths::from_env().project.is_some() {
            "project"
        } else {
            "global"
        };
        let scoped_prefs = load_scoped_panel_prefs(settings_scope, selected_from_sqlite.clone());
        let defaults = scoped_prefs
            .as_ref()
            .map(|prefs| prefs.defaults.clone())
            .unwrap_or_else(|| load_panel_prefs(selected_from_sqlite));
        panel.sync_runtime_defaults(&defaults);
        if let Some(selected_thread_id) = defaults.selected_thread_id {
            if let Some(real_idx) = panel.bridge.as_ref().and_then(|bridge| {
                (0..panel.model.borrow().threads.len()).find(|idx| {
                    bridge
                        .thread_binding(*idx)
                        .is_some_and(|binding| binding.thread_id == selected_thread_id)
                })
            }) {
                if let Some(filtered_idx) = panel
                    .model
                    .borrow()
                    .visible_indices
                    .iter()
                    .position(|idx| *idx == real_idx)
                {
                    dispatch::dispatch_thread_selected(&panel, filtered_idx);
                }
            }
        }
        panel.dispatch_frame_input(crate::msg::FrameInput {
            settings_preferences_snapshot: Some(
                crate::external_snapshot::ExternalSnapshotSource::new(&panel)
                    .collect_settings_preferences_snapshot(Some(settings_scope)),
            ),
            ..crate::msg::FrameInput::default()
        });
        let dev_mode_at_startup = panel.model.borrow().dev_mode;
        if dev_mode_at_startup {
            // Mirrors on_dev_mode_toggled's install-on-enable behavior --
            // that callback only fires on the OFF->ON transition, so a
            // fresh launch that loads dev_mode already persisted `true`
            // never got the bundled default skill installed at all,
            // leaving dev mode on with zero global skills to show.
            let global_dir = crate::skills_state::global_skills_dir(&resolve_cache_dir());
            if let Err(error) = crate::skills_state::ensure_bundled_global_skill(&global_dir) {
                eprintln!("panel-rust: failed to install bundled global skill at startup: {error}");
            }
            panel.dispatch_frame_input(crate::msg::FrameInput {
                skills_snapshot: Some(
                    crate::external_snapshot::ExternalSnapshotSource::new(&panel)
                        .collect_skills_snapshot(),
                ),
                ..crate::msg::FrameInput::default()
            });
        }
        if let Some(real_idx) = panel.real_index(panel.model.borrow().selected_thread) {
            panel.dispatch_frame_input(crate::msg::FrameInput {
                selected_thread_snapshot: crate::external_snapshot::ExternalSnapshotSource::new(
                    &panel,
                )
                .collect_thread_snapshot_for(real_idx),
                ..crate::msg::FrameInput::default()
            });
        } else {
            panel.dispatch_frame_input(crate::msg::FrameInput {
                clear_selected_thread: true,
                ..crate::msg::FrameInput::default()
            });
        }

        // Thread callbacks enter through Msg::Ui(UiMsg::Thread(..)).
        let component_weak = panel.component.as_weak();
        panel.component.on_thread_selected(move |idx| {
            let Some(_component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    // `idx` is a filtered-list index (Phase 2).
                    dispatch::dispatch_thread_selected(panel, idx as usize);
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel
            .component
            .on_thread_navigation_requested(move |delta| {
                let Some(_component) = component_weak.upgrade() else {
                    return;
                };
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_thread_navigate(panel, delta);
                    }
                });
            });

        // tea-slint-model Phase 4 (Settings domain): routed through
        // Msg::Ui(UiMsg::Settings(..)) -> update() -> dispatch's bridge
        // into the settings, MCP, profile, and agent dispatch methods
        // (unchanged, now pub(crate)) -- see dispatch.rs's doc comment.
        let component_weak = panel.component.as_weak();
        panel.component.on_settings_requested(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_settings_open(panel, &component);
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_settings_scope_changed(move |scope| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_settings_scope_changed(panel, &component, scope.to_string());
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
                    dispatch::dispatch_settings_save(panel, &component);
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_settings_close(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_settings_close(panel, &component);
                }
            });
        });

        panel.component.on_error_banner_dismissed(move || {
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_error_banner_dismissed(panel);
                }
            });
        });

        panel
            .component
            .on_thread_toggle_background(move |slint_index| {
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_thread_toggle_background(panel, slint_index as usize);
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
                    dispatch::dispatch_mcp_server_create(
                        panel,
                        &component,
                        name.to_string(),
                        command.to_string(),
                    );
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
                    dispatch::dispatch_mcp_server_delete(panel, &component, name.to_string());
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel
            .component
            .on_mcp_server_enabled_changed(move |name, enabled| {
                let Some(component) = component_weak.upgrade() else {
                    return;
                };
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_mcp_server_enabled_changed(
                            panel,
                            &component,
                            name.to_string(),
                            enabled,
                        );
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
                        dispatch::dispatch_profile_create(
                            panel,
                            &component,
                            name.to_string(),
                            (!agent_id.is_empty()).then(|| agent_id.to_string()),
                            terminal_enabled,
                            fs_enabled,
                        );
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
                    dispatch::dispatch_profile_delete(panel, &component, name.to_string());
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
                    dispatch::dispatch_agent_install_requested(
                        panel,
                        &component,
                        agent_id.to_string(),
                    );
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
                    if let Some(panel) = slot.as_mut() {
                        dispatch::dispatch_thread_recover_session_attach(
                            panel,
                            &component,
                            session_id.to_string(),
                            provider.to_string(),
                            title.to_string(),
                        );
                    }
                });
            });

        let component_weak = panel.component.as_weak();
        panel.component.on_new_thread_requested(move || {
            PANEL.with(|cell| {
                let mut slot = cell.borrow_mut();
                if let Some(panel) = slot.as_mut() {
                    let Some(component) = component_weak.upgrade() else {
                        return;
                    };
                    dispatch::dispatch_thread_new(panel, &component);
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel
            .component
            .on_thread_rename_requested(move |filtered_idx, name| {
                let Some(component) = component_weak.upgrade() else {
                    return;
                };
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_thread_rename(
                            panel,
                            &component,
                            filtered_idx as usize,
                            name.to_string(),
                        );
                    }
                });
            });

        let component_weak = panel.component.as_weak();
        panel
            .component
            .on_thread_close_requested(move |filtered_idx| {
                let Some(component) = component_weak.upgrade() else {
                    return;
                };
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_thread_close(panel, &component, filtered_idx as usize);
                    }
                });
            });

        let component_weak = panel.component.as_weak();
        panel
            .component
            .on_thread_delete_requested(move |filtered_idx| {
                let Some(component) = component_weak.upgrade() else {
                    return;
                };
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_thread_delete(panel, &component, filtered_idx as usize);
                    }
                });
            });

        panel.component.on_new_skill_requested(move |name, scope| {
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_new_skill_requested(
                        panel,
                        name.to_string(),
                        scope.to_string(),
                    );
                }
            });
        });

        panel.component.on_skill_promote_to_global(move |path| {
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_skill_promote_to_global(panel, path.to_string());
                }
            });
        });

        panel.component.on_dev_mode_toggled(move |enabled| {
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_dev_mode_toggled(panel, enabled);
                }
            });
        });

        panel.component.on_skill_editor_open_requested(move |path| {
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_skill_editor_open_requested(panel, path.to_string());
                }
            });
        });

        panel
            .component
            .on_skill_content_edited(move |path, content| {
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_skill_content_edited(
                            panel,
                            path.to_string(),
                            content.to_string(),
                        );
                    }
                });
            });

        panel.component.on_skill_copy_path_requested(move |path| {
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_skill_copy_path_requested(panel, path.to_string());
                }
            });
        });

        panel
            .component
            .on_skill_open_in_editor_requested(move |editor_name, path| {
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_skill_open_in_editor_requested(
                            panel,
                            editor_name.to_string(),
                            path.to_string(),
                        );
                    }
                });
            });

        panel
            .component
            .on_skill_open_with_os_default_requested(move |path| {
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_skill_open_with_os_default_requested(
                            panel,
                            path.to_string(),
                        );
                    }
                });
            });

        // tea-slint-model Phase 4 (Compose domain): routed through
        // Msg::Ui(UiMsg::Compose(..)) -> update() -> dispatch's bridge
        // into the reducer and effect executor -- see dispatch.rs's
        // doc comment.
        let component_weak = panel.component.as_weak();
        panel.component.on_send_requested(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            let text = component.get_compose_text().to_string();
            let text = text.trim().to_owned();
            if text.is_empty() {
                trace_host_input("send requested with empty composer");
                return;
            }
            let filtered_idx = component.get_selected_thread() as usize;
            trace_host_input(format_args!(
                "send requested selected_thread={filtered_idx} text={text:?}"
            ));
            PANEL.with(move |cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_compose_send(panel, filtered_idx, text);
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_stop_requested(move || {
            let Some(_component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_compose_stop(panel);
                }
            });
        });

        // Interactive agent-request relay addition: approve/reject
        // buttons on the request card. Both handlers re-read the exact
        // `AgentRequestEvent` from
        // `AgentBridge::pending_requests` (rather than trusting only the
        // Slint-side `PendingRequestItem` snapshot's `relay-id` string)
        // so `permission::build_response` gets the real, untruncated
        // `raw_request` needed to build a native `session/request_
        // permission` reply -- the Slint struct only carries a
        // human-readable summary, not the full JSON.
        // tea-slint-model Phase 4 (Request domain): routed through
        // Msg::Ui(UiMsg::Request(..)) -> update() -> dispatch's bridge
        // into answer_pending_request/answer_pending_request_option/
        // dispatch_load_older_requested (unchanged, now pub(crate)) --
        // see dispatch.rs's doc comment.
        let component_weak = panel.component.as_weak();
        panel.component.on_approve_request(move || {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_request_approve(panel, &component);
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
                    dispatch::dispatch_request_reject(panel, &component);
                }
            });
        });

        // One-of select: each option row on the permission card sends its
        // optionId (ACP or synthetic approve/reject).
        let component_weak = panel.component.as_weak();
        panel
            .component
            .on_permission_option_selected(move |option_id| {
                let Some(component) = component_weak.upgrade() else {
                    return;
                };
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_request_permission_option(
                            panel,
                            &component,
                            option_id.to_string(),
                        );
                    }
                });
            });

        // Terminal-view addition: expand a card into the floating
        // overlay, and close it. Selected-thread `FrameInput` snapshots keep
        // whichever terminal is
        // currently expanded live-updating; these two callbacks only
        // own which id (if any) is expanded.
        // tea-slint-model Phase 4 (Terminal domain): routed through
        // Msg::Ui(UiMsg::Terminal(..)) -> update() -> dispatch's bridge
        // into the dispatch_expand_terminal/dispatch_close_terminal_overlay/
        // dispatch_local_terminal_toggle/dispatch_local_terminal_key_input/
        // dispatch_local_terminal_close methods (unchanged, now
        // pub(crate)) -- see dispatch.rs's doc comment.
        let component_weak = panel.component.as_weak();
        panel.component.on_expand_terminal(move |terminal_id| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_terminal_expand(panel, &component, terminal_id.to_string());
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
                    dispatch::dispatch_terminal_close_overlay(panel);
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
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_terminal_local_toggle(panel, &component);
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
                    dispatch::dispatch_terminal_local_key_input(
                        panel,
                        &component,
                        text.to_string(),
                    );
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
                    dispatch::dispatch_terminal_local_close(panel, &component);
                }
            });
        });

        // Mode/config selector addition: dispatch `session/set_mode`/
        // `session/set_config_option` on the *currently displayed*
        // thread. Neither callback optimistically updates `current-
        // mode-id`/`config-option-rows` itself -- both wait for the
        // real backend's own confirmation (`AgentEvent::
        // CurrentModeChanged`/`ConfigOptions`, applied by `apply_bridge_
        // events` -> the FrameInput capability projection), matching `AgentBridge::
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
                    dispatch::dispatch_mode_selected(panel, &component, mode_id.to_string());
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
                        dispatch::dispatch_config_option_selected(
                            panel,
                            &component,
                            option_id.to_string(),
                            value.to_string(),
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
                    dispatch::dispatch_search_changed(panel, &component, query.to_string());
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel
            .component
            .on_search_submitted(move |query, search_skills, show_global| {
                let Some(component) = component_weak.upgrade() else {
                    return;
                };
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_search_submitted(
                            panel,
                            &component,
                            query.to_string(),
                            search_skills,
                            show_global,
                        );
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
                    dispatch::dispatch_toggle_expanded(panel, index as usize);
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
                    dispatch::dispatch_request_load_older(panel, &component);
                }
            });
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

/// Whether *any* editable Slint surface currently owns focus -- the
/// composer, a local PTY terminal, or a secondary text input (thread
/// search, skill search, settings search, dropdown filters, the mention
/// popup -- see `secondary_text_input_has_focus`'s doc comment above for
/// the full OR-chain). Queryable independent of an actual key event, so
/// the host can decide *before* Qt's shortcut dispatch runs (a
/// `QEvent::ShortcutOverride` handler, see `RustPanelItem::event` in
/// rustpanelitem.cpp) whether a single-key host shortcut (e.g. Shotcut's
/// bare "A" for Append, "/" for its own binding) should be allowed to
/// fire, or must be suppressed so the key reaches the focused Slint
/// surface as ordinary typed text instead. Without this, a real, reported
/// bug: typing "a" or "/" into the chat box (or, by the same mechanism,
/// into thread search) instead triggered Shotcut's own action (observed
/// live: bare "/" opened Shotcut's own Keyboard Shortcuts editor) and
/// never reached the focused Slint TextInput at all.
#[no_mangle]
pub extern "C" fn panel_rust_has_text_focus(_handle: *mut PanelHandle) -> bool {
    PANEL.with(|cell| {
        cell.borrow().as_ref().is_some_and(|panel| {
            panel.component.get_compose_has_focus()
                || panel.component.get_local_terminal_has_focus()
                || panel.component.get_secondary_text_input_has_focus()
        })
    })
}

/// Maps a `CursorHost.kind` string (set declaratively by every interactive
/// component's `has-hover`/`has-focus` change-handler -- see
/// `ui/tokens/cursor_host.slint` for why this indirection exists instead of
/// Slint's own internal cursor-shape tracking) to a `Qt::CursorShape` enum
/// value, so `RustPanelItem::poll()` (rustpanelitem.cpp) can call
/// `setCursor(static_cast<Qt::CursorShape>(shape))` directly -- the same
/// "map Qt-specific values on the Rust side" convention `map_qt_key` already
/// uses for keyboard input.
fn qt_cursor_shape_for_kind(kind: &str) -> c_int {
    match kind {
        "pointer" => 13, // Qt::PointingHandCursor
        "text" => 4,     // Qt::IBeamCursor
        _ => 0,          // Qt::ArrowCursor
    }
}

#[no_mangle]
pub extern "C" fn panel_rust_cursor_shape(_handle: *mut PanelHandle) -> c_int {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return 0; // Qt::ArrowCursor
        };
        qt_cursor_shape_for_kind(panel.component.global::<CursorHost>().get_kind().as_str())
    })
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
            .and_then(|idx| {
                panel
                    .model
                    .borrow()
                    .threads
                    .get(idx)
                    .map(|thread| thread.state.clone())
            })
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

/// Forwards hover-only mouse movement (no button held) at physical pixel
/// coordinates. Without this, a `TouchArea`'s `has-hover` (the shared
/// `Button`/`IconButton` components' hover-tinted background, and any
/// `mouse-cursor` binding) never updates at all outside of a
/// press/release, since Slint only learns about pointer position via
/// explicit `WindowEvent::PointerMoved` dispatches -- `panel_rust_input_click`
/// already sends one immediately before its own Press, but that's the only
/// place any `PointerMoved` was ever dispatched before this. Real bug this
/// closes (tasks/v2/enhance.yaml#task-4): "hover effects... cursor change,
/// the ui components picking hover... are not propagated" -- confirmed via
/// direct inspection that `RustPanelItem` (rustpanelitem.cpp) never called
/// `setAcceptHoverEvents(true)` nor overrode `hoverMoveEvent` at all, so Qt
/// never even told this item about mouse movement without a button down.
#[no_mangle]
pub extern "C" fn panel_rust_input_hover(_handle: *mut PanelHandle, x: c_uint, y: c_uint) -> bool {
    let window = PANEL.with(|cell| cell.borrow().as_ref().map(|panel| panel.window.clone()));
    let Some(window) = window else {
        return false;
    };
    window.window().dispatch_event(WindowEvent::PointerMoved {
        position: slint::LogicalPosition::new(x as f32, y as f32),
    });
    true
}

/// Forwards the pointer leaving the panel's bounds entirely (Qt's
/// `hoverLeaveEvent`), so any `has-hover` state correctly clears instead of
/// staying stuck at whatever it was under the last position inside the
/// panel that ever received a move event.
#[no_mangle]
pub extern "C" fn panel_rust_input_hover_exit(_handle: *mut PanelHandle) -> bool {
    let window = PANEL.with(|cell| cell.borrow().as_ref().map(|panel| panel.window.clone()));
    let Some(window) = window else {
        return false;
    };
    window.window().dispatch_event(WindowEvent::PointerExited);
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
    window
        .window()
        .dispatch_event(WindowEvent::PointerScrolled {
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
    // Raw `Qt::KeyboardModifiers` bitmask (`QKeyEvent::modifiers()`,
    // forwarded verbatim by the caller) -- only bit 0x02000000
    // (`Qt::ShiftModifier`) is currently consulted, by `map_qt_key`'s
    // empty-text fallback for deciding a letter's case (`Qt::Key_A`..
    // `Key_Z` are case-insensitive constants, so that decision is
    // otherwise unrecoverable from `qt_key` alone -- see that function's
    // own doc comment).
    modifiers: c_int,
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
    PANEL.with(|cell| {
        if let Some(panel) = cell.borrow().as_ref() {
            dispatch::dispatch_host_input_key(panel, text.to_owned(), modifiers as u32);
        }
    });
    // The host must not consume editor shortcuts unless an editable Slint
    // surface owns focus. Besides the composer, a local PTY terminal is a
    // genuine keyboard target and must receive printable keys, editing keys,
    // and arrows without Shotcut handling them as global shortcuts.
    let (compose_has_focus, local_terminal_has_focus, secondary_text_input_has_focus) =
        PANEL.with(|cell| {
            cell.borrow()
                .as_ref()
                .map_or((false, false, false), |panel| {
                    (
                        panel.component.get_compose_has_focus(),
                        panel.component.get_local_terminal_has_focus(),
                        panel.component.get_secondary_text_input_has_focus(),
                    )
                })
        });
    // `secondary_text_input_has_focus` covers every editable Slint surface
    // besides the composer/terminal (thread search, skill search -- see
    // app.slint's own doc comment on that property for the full list and
    // why a field left out of its OR-chain silently drops all keystrokes
    // here). Without it, clicking into e.g. thread search focuses it fine
    // (a real click) but every subsequent keystroke was dropped right here
    // before ever reaching Slint -- the search box "didn't take input at
    // all" despite compiling and rendering correctly.
    if !compose_has_focus && !local_terminal_has_focus && !secondary_text_input_has_focus {
        trace_host_input(format_args!(
            "key qt_key={qt_key:#x} pressed={pressed} text={text:?} \
             compose_focus=false local_terminal_focus=false secondary_focus=false"
        ));
        return false;
    }
    // TextInput consumes text on key press. Forwarding Qt's matching release
    // with the same text can make a character appear twice in an embedded
    // host, so consume releases after the focus guard without redispatching
    // their text to Slint -- EXCEPT for the bare modifier keys `map_qt_key`
    // now maps on press (Shift/Control/Meta/Alt). Those aren't text at all,
    // and Slint's internal modifier tracking (`InternalKeyboardModifierState`)
    // only clears a modifier on a matching `KeyReleased`; without forwarding
    // this, a modifier would look permanently "held" in Slint after the
    // very first press, since this bridge otherwise never sends releases.
    if !pressed {
        if let Some(key) = modifier_key_for_qt_key(qt_key) {
            trace_host_input(format_args!(
                "key qt_key={qt_key:#x} pressed=false text={text:?} \
                 compose_focus={compose_has_focus} local_terminal_focus={local_terminal_has_focus} secondary_focus={secondary_text_input_has_focus} \
                 modifier_release={key:?}"
            ));
            window.window().dispatch_event(WindowEvent::KeyReleased {
                text: SharedString::from(key),
            });
            return true;
        }
        trace_host_input(format_args!(
            "key qt_key={qt_key:#x} pressed=false text={text:?} \
             compose_focus={compose_has_focus} local_terminal_focus={local_terminal_has_focus} secondary_focus={secondary_text_input_has_focus}"
        ));
        return true;
    }
    const QT_SHIFT_MODIFIER: c_int = 0x0200_0000;
    let shift = (modifiers & QT_SHIFT_MODIFIER) != 0;
    let Some(key_text) = map_qt_key(qt_key, text, shift) else {
        trace_host_input(format_args!(
            "key qt_key={qt_key:#x} pressed=true text={text:?} \
             compose_focus={compose_has_focus} local_terminal_focus={local_terminal_has_focus} secondary_focus={secondary_text_input_has_focus} \
             mapped=false"
        ));
        return false;
    };
    trace_host_input(format_args!(
        "key qt_key={qt_key:#x} pressed=true text={text:?} \
         compose_focus={compose_has_focus} local_terminal_focus={local_terminal_has_focus} secondary_focus={secondary_text_input_has_focus} \
         mapped={key_text:?}"
    ));
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: key_text });
    true
}

/// Command ids for [`panel_rust_invoke_command`]. Kept in sync with the C++
/// side's own constants in `rustpanelitem.cpp` -- there is no shared header
/// enum because this crate's `cbindgen`-style boundary is plain `extern
/// "C"` functions, matching every other entry point in this file.
const PANEL_COMMAND_PREVIOUS_THREAD: c_int = 0;
const PANEL_COMMAND_NEXT_THREAD: c_int = 1;
const PANEL_COMMAND_OPEN_THREAD_SEARCH: c_int = 2;

/// Narrow, focus-independent command dispatch for host-side global
/// shortcuts (switch thread, open thread search) that must work even when
/// neither the compose box nor a local terminal owns Slint focus --
/// `panel_rust_input_key` above intentionally drops everything in that
/// case (see its focus guard) so Shotcut's own bare-letter shortcuts don't
/// get eaten while, say, the sidebar merely has Qt focus. This function is
/// the escape hatch: it goes straight to the same Slint callbacks the
/// in-panel Ctrl+Alt+Up/Down and Ctrl+K bindings use
/// (`thread-navigation-requested` / `open-thread-search` in app.slint), so
/// there is exactly one implementation of "switch thread" / "open search"
/// regardless of which input path triggered it.
#[no_mangle]
pub extern "C" fn panel_rust_invoke_command(_handle: *mut PanelHandle, command: c_int) -> bool {
    PANEL.with(|cell| {
        cell.borrow()
            .as_ref()
            .is_some_and(|panel| dispatch::dispatch_host_invoke_command(panel, command))
    })
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
        dispatch::dispatch_theme_changed(panel, text.to_owned());
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
    let path = if path_ptr.is_null() || path_len == 0 {
        None
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(path_ptr, path_len) };
        std::str::from_utf8(bytes).ok().map(str::to_string)
    };
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        dispatch::dispatch_project_path_changed(panel, path);
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
        dispatch::dispatch_apply_host_appearance(panel, appearance)
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
    // Slint `animate` blocks (hover fades, entrance/exit transitions, the
    // loading spinner, the sidebar rail's `animate width`, ...) -- and a
    // `Flickable`'s own interactive flick/momentum motion -- only progress
    // when something calls this. Under a real windowing backend the
    // platform event loop does it automatically, but this crate's
    // `SpikePlatform`/`MinimalSoftwareWindow` has no event loop of its own,
    // only this QTimer poll (rustpanelitem.cpp's RustPanelItem::poll,
    // interval adaptive to the real display refresh rate, 60-90fps --
    // see updatePollIntervalForRefreshRate()). Without this call every
    // `animate` was simply frozen -- properties
    // jumped straight to their end value with no interpolation, and a
    // Flickable's drag-then-release momentum never advanced either, since
    // nothing ever advanced Slint's animation clock. Called unconditionally,
    // every tick, regardless of whatever else below finds "changed" -- an
    // in-flight animation is itself a reason to redraw even with zero
    // application-state change this tick.
    slint::platform::update_timers_and_animations();
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        dispatch::dispatch_frame_poll(panel) || panel.window.window().has_active_animations()
    })
}

// Below this, a real layout pass squeezing this component's full nested
// item tree (sidebar + chat area, many icons/rows deep) into a
// near-zero canvas can produce a degenerate (effectively-zero or
// precision-lost) destination size for some nested `Image` item --
// `i_slint_renderer_software`'s `draw_image_impl` then fails an internal
// `euclid::Size2D` numeric cast and, since this crate builds with
// `panic = "abort"`, that panic takes down the whole host process
// instead of unwinding (confirmed via a real crash: `RustPanelItem::
// paint` -> `panel_rust_render` -> deep `visit_children_item` recursion
// -> `draw_image_impl` -> `Size2D::cast().unwrap()` on `None`, on the
// very first paint of a freshly-created dock before Qt's own layout has
// given it its real ~20%-of-window size). The host (`rustpanelitem.cpp`)
// only floors width/height at `qMax(1.0, ...)`, so a literal 1x1 first
// paint is possible and was in fact what triggered this. Skipping the
// render entirely below this floor is harmless: Qt repaints again as
// soon as the item's real geometry lands, typically within the same
// event-loop tick.
const MIN_RENDERABLE_SIZE: u32 = 16;

#[no_mangle]
pub extern "C" fn panel_rust_render(_handle: *mut PanelHandle) -> bool {
    panel_rust_render_impl()
}

fn panel_rust_render_impl() -> bool {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        let width = panel.width;
        if width < MIN_RENDERABLE_SIZE || panel.height < MIN_RENDERABLE_SIZE {
            return false;
        }
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
    fn cursor_shape_maps_known_kinds_to_qt_enum_values() {
        assert_eq!(qt_cursor_shape_for_kind("pointer"), 13); // Qt::PointingHandCursor
        assert_eq!(qt_cursor_shape_for_kind("text"), 4); // Qt::IBeamCursor
    }

    #[test]
    fn cursor_shape_defaults_to_arrow_for_default_and_unknown_kinds() {
        assert_eq!(qt_cursor_shape_for_kind("default"), 0); // Qt::ArrowCursor
        assert_eq!(qt_cursor_shape_for_kind(""), 0);
        assert_eq!(qt_cursor_shape_for_kind("some-future-kind"), 0);
    }

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
        assert!(panel_rust_render_impl());
        assert!(panel_rust_input_scroll(first, 48.0, 32.0, 0.0, -40.0));
        PANEL.with(|cell| {
            let panel = cell.borrow();
            let panel = panel.as_ref().expect("panel exists");
            panel
                .component
                .set_compose_text("preserve this draft".into());
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
            let model = panel.model.borrow();
            let appearance = &model.appearance;
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

#[cfg(test)]
mod keyboard_shortcut_tests {
    use super::*;
    use i_slint_backend_testing::ElementHandle;

    /// Forces the bridge into its documented display-only fallback (see
    /// `lifecycle_tests`) and constructs a real panel via `panel_rust_
    /// create` -- the actual production entry point, not a bare `ChatPanel::
    /// new()` -- so `on_thread_navigation_requested`/`on_thread_selected`
    /// are really wired the way they are in the shipped app. Returns a
    /// guard that restores the previous env vars on drop.
    struct TestPanel {
        handle: *mut PanelHandle,
        previous_env: Vec<(&'static str, Option<String>)>,
        _cache_dir: tempfile::TempDir,
    }

    impl TestPanel {
        fn new() -> Self {
            let cache_dir = tempfile::tempdir().expect("cache dir");
            let previous_env = [
                (
                    "RUI_ACPX_CODEX_URL",
                    std::env::var("RUI_ACPX_CODEX_URL").ok(),
                ),
                (
                    "RUI_ACPX_CLAUDE_URL",
                    std::env::var("RUI_ACPX_CLAUDE_URL").ok(),
                ),
                ("RUI_ACP_CACHE_DIR", std::env::var("RUI_ACP_CACHE_DIR").ok()),
            ]
            .to_vec();
            std::env::set_var("RUI_ACPX_CODEX_URL", "http://127.0.0.1:1");
            std::env::set_var("RUI_ACPX_CLAUDE_URL", "http://127.0.0.1:1");
            std::env::set_var("RUI_ACP_CACHE_DIR", cache_dir.path());

            let handle = panel_rust_create(240, 260);
            assert!(!handle.is_null());
            Self {
                handle,
                previous_env,
                _cache_dir: cache_dir,
            }
        }

        fn component(&self) -> ChatPanel {
            PANEL.with(|cell| {
                cell.borrow()
                    .as_ref()
                    .expect("panel exists")
                    .component
                    .clone_strong()
            })
        }

        /// Sets the Slint `threads` model *and* the Rust-side `visible_
        /// indices` it's paired with in real production code (the model
        /// projection updates both together next
        /// to its own `set_threads` call) -- the dispatcher clamps against
        /// `visible_indices`, not the Slint model, so setting only
        /// `threads` directly (bypassing the real bridge-driven population
        /// pipeline this test doesn't spin up) would leave it stale/empty
        /// and silently break navigation.
        fn set_threads(&self, threads: Vec<ThreadItem>) {
            let count = threads.len();
            PANEL.with(|cell| {
                let slot = cell.borrow();
                let panel = slot.as_ref().expect("panel exists");
                let keys: Vec<String> = (0..count).map(|idx| format!("thread:{idx}")).collect();
                crate::list_model::reconcile(
                    &panel.model.borrow().thread_model,
                    &mut panel.model.borrow().thread_model_keys.borrow_mut(),
                    &keys,
                    &threads,
                );
                panel.model.borrow_mut().visible_indices = (0..count).collect();
            });
        }

        /// Presses then releases `qt_key` through the real `panel_rust_
        /// input_key` FFI boundary -- the literal function `RustPanelItem::
        /// keyPressEvent`/`keyReleaseEvent` call in the shipped C++ host,
        /// not a direct `WindowEvent` dispatch that would bypass `map_qt_
        /// key`'s Qt -> Slint translation entirely.
        fn press_and_release(&self, qt_key: c_int, text: &str) {
            let bytes = text.as_bytes();
            panel_rust_input_key(
                self.handle,
                qt_key,
                bytes.as_ptr(),
                bytes.len(),
                /*pressed=*/ true,
                0,
            );
            panel_rust_input_key(
                self.handle,
                qt_key,
                bytes.as_ptr(),
                bytes.len(),
                /*pressed=*/ false,
                0,
            );
        }

        /// Holds `qt_key` down without releasing it -- for modifier keys,
        /// used to build a real chord before pressing the "real" key.
        fn press_only(&self, qt_key: c_int) {
            panel_rust_input_key(self.handle, qt_key, std::ptr::null(), 0, true, 0);
        }

        fn release_only(&self, qt_key: c_int) {
            panel_rust_input_key(self.handle, qt_key, std::ptr::null(), 0, false, 0);
        }
    }

    impl Drop for TestPanel {
        fn drop(&mut self) {
            panel_rust_destroy(self.handle);
            for (key, value) in self.previous_env.drain(..) {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }

    const QT_KEY_CONTROL: c_int = 0x0100_0021;
    const QT_KEY_ALT: c_int = 0x0100_0023;
    const QT_KEY_UP: c_int = 0x0100_0013;
    const QT_KEY_DOWN: c_int = 0x0100_0015;
    const QT_KEY_K: c_int = 0x4b;

    fn thread_item(name: &str) -> ThreadItem {
        ThreadItem {
            name: name.into(),
            status: "idle".into(),
            busy: false,
            open: true,
            background: false,
            description: "".into(),
            closed: false,
            provider: "".into(),
            model: "".into(),
            project_name: "".into(),
            project_path: "".into(),
        }
    }

    /// Real Ctrl+Alt+Up/Down and Ctrl+K chords, driven through the actual
    /// `panel_rust_input_key` FFI boundary (the function C++'s
    /// `RustPanelItem::keyPressEvent`/`keyReleaseEvent` call), with the
    /// composer focused -- the "AI chat has focus" case. Exercises the
    /// full real path: Qt key codes -> `map_qt_key` -> Slint `KeyPressed`
    /// -> `panel-keys`/composer `panel-shortcut` re-dispatch ->
    /// `handle-panel-shortcut` -> `thread-navigation-requested`/`open-
    /// thread-search` -> the real `on_thread_navigation_requested`/`on_
    /// thread_selected` Rust handlers -> reducer/dispatcher selection.
    #[test]
    fn ctrl_alt_arrows_and_ctrl_k_work_through_the_real_input_key_bridge() {
        let panel = TestPanel::new();
        let component = panel.component();

        panel.set_threads(vec![
            thread_item("Fix timeline crash"),
            thread_item("Render title card"),
            thread_item("Draft narration script"),
        ]);
        component.set_selected_thread(0);
        component.set_sidebar_expanded(false);

        // `panel_rust_create` installs the real production `SpikePlatform`,
        // not `i_slint_backend_testing`'s mock-time testing platform, so
        // `ElementHandle::mock_single_click` (which needs the latter)
        // panics here with "the platform's clock is not monotonic". Drive
        // focus through `panel_rust_input_click` instead -- the same
        // real-click FFI a genuine mouse click goes through in the shipped
        // app -- at the compose box's real on-screen center.
        let compose = ElementHandle::find_by_accessible_label(&component, "Compose message")
            .next()
            .expect("compose input must be accessible");
        let position = compose.absolute_position();
        let size = compose.size();
        assert!(
            panel_rust_input_click(
                panel.handle,
                (position.x + size.width / 2.0) as c_uint,
                (position.y + size.height / 2.0) as c_uint,
            ),
            "click on the composer must reach the real input-click FFI"
        );
        assert!(
            component.get_compose_has_focus(),
            "composer must accept real focus before dispatching chords"
        );

        // Ctrl+Alt+Down: next thread.
        panel.press_only(QT_KEY_CONTROL);
        panel.press_only(QT_KEY_ALT);
        panel.press_and_release(QT_KEY_DOWN, "");
        panel.release_only(QT_KEY_ALT);
        panel.release_only(QT_KEY_CONTROL);
        assert_eq!(
            component.get_selected_thread(),
            1,
            "Ctrl+Alt+Down through the real FFI boundary must advance to the next thread"
        );

        // Wraps past the end back to the first thread.
        panel.press_only(QT_KEY_CONTROL);
        panel.press_only(QT_KEY_ALT);
        panel.press_and_release(QT_KEY_DOWN, "");
        panel.press_and_release(QT_KEY_DOWN, "");
        panel.release_only(QT_KEY_ALT);
        panel.release_only(QT_KEY_CONTROL);
        assert_eq!(
            component.get_selected_thread(),
            0,
            "Ctrl+Alt+Down must wrap from the last thread back to the first"
        );

        // Ctrl+Alt+Up: previous thread, wrapping the other direction.
        panel.press_only(QT_KEY_CONTROL);
        panel.press_only(QT_KEY_ALT);
        panel.press_and_release(QT_KEY_UP, "");
        panel.release_only(QT_KEY_ALT);
        panel.release_only(QT_KEY_CONTROL);
        assert_eq!(
            component.get_selected_thread(),
            2,
            "Ctrl+Alt+Up must wrap from the first thread back to the last"
        );

        assert_eq!(
            component.get_compose_text(),
            "",
            "the chord must not leak arrow-key text into the composer"
        );

        // Released modifiers must not stay "stuck" held in Slint's
        // internal tracking -- a plain Down arrow now (no modifiers) must
        // NOT be treated as another thread-switch chord.
        panel.press_and_release(QT_KEY_DOWN, "");
        assert_eq!(
            component.get_selected_thread(),
            2,
            "a bare Down arrow after releasing Ctrl+Alt must not still switch threads"
        );

        // Ctrl+K: opens/focuses thread search, expanding the collapsed
        // rail first -- observable end-to-end via sidebar-expanded.
        assert!(!component.get_sidebar_expanded());
        panel.press_only(QT_KEY_CONTROL);
        panel.press_and_release(QT_KEY_K, "k");
        panel.release_only(QT_KEY_CONTROL);
        assert!(
            component.get_sidebar_expanded(),
            "Ctrl+K through the real FFI boundary must expand the thread rail to reach search"
        );
    }

    /// The focus-independent path a real C++ host takes when the panel has
    /// no Qt focus at all (`panel_rust_input_key`'s own focus guard drops
    /// everything in that case -- see its doc comment) or when Shotcut's
    /// global `ChatRustDock` `QShortcut`s fire: `panel_rust_invoke_command`,
    /// with no composer focus and no key events at all.
    #[test]
    fn invoke_command_switches_threads_and_opens_search_without_any_focus() {
        let panel = TestPanel::new();
        let component = panel.component();

        panel.set_threads(vec![
            thread_item("Fix timeline crash"),
            thread_item("Render title card"),
        ]);
        component.set_selected_thread(0);
        component.set_sidebar_expanded(false);
        assert!(
            !component.get_compose_has_focus(),
            "this path must not require composer focus"
        );

        assert!(panel_rust_invoke_command(
            panel.handle,
            PANEL_COMMAND_NEXT_THREAD
        ));
        assert_eq!(component.get_selected_thread(), 1);

        assert!(panel_rust_invoke_command(
            panel.handle,
            PANEL_COMMAND_PREVIOUS_THREAD
        ));
        assert_eq!(component.get_selected_thread(), 0);

        assert!(panel_rust_invoke_command(
            panel.handle,
            PANEL_COMMAND_OPEN_THREAD_SEARCH
        ));
        assert!(component.get_sidebar_expanded());
    }

    /// `panel_rust_has_text_focus`'s OR-chain (compose / local terminal /
    /// secondary text input) via the real click-focus path, not a direct
    /// property poke -- covers the compose-box arm live and the other two
    /// arms by construction (same `||` expression, already covered
    /// individually by `secondary_text_input_has_focus`'s own OR-chain in
    /// app.slint) plus the no-focus-at-all false case.
    #[test]
    fn has_text_focus_reflects_real_compose_focus_state() {
        let panel = TestPanel::new();
        let component = panel.component();

        assert!(
            !panel_rust_has_text_focus(panel.handle),
            "no editable surface has been focused yet"
        );

        let compose = ElementHandle::find_by_accessible_label(&component, "Compose message")
            .next()
            .expect("compose input must be accessible");
        let position = compose.absolute_position();
        let size = compose.size();
        assert!(panel_rust_input_click(
            panel.handle,
            (position.x + size.width / 2.0) as c_uint,
            (position.y + size.height / 2.0) as c_uint,
        ));
        assert!(component.get_compose_has_focus());
        assert!(
            panel_rust_has_text_focus(panel.handle),
            "compose focus must be reflected through the has-text-focus FFI"
        );
    }

    /// Regression test for the confirmed bug where real Ctrl+<letter>
    /// combos deliver `QKeyEvent::text()` as an ASCII control character
    /// (e.g. Ctrl+B -> "\u{2}"), which `map_qt_key` must normalize back to
    /// the plain letter -- otherwise `handle-panel-shortcut`'s `event.text
    /// == "b"`-style checks (and TextInput's own built-in Ctrl+A
    /// select-all) never match through the real host bridge. Drives the
    /// actual control-character bytes through `panel_rust_input_key`, the
    /// same call `RustPanelItem::keyPressEvent` makes, rather than passing
    /// the already-normalized letter directly.
    #[test]
    fn ctrl_b_through_the_real_input_key_bridge_toggles_the_sidebar() {
        const QT_KEY_B: c_int = 0x42;

        let panel = TestPanel::new();
        let component = panel.component();
        panel.set_threads(vec![thread_item("Fix timeline crash")]);
        component.set_selected_thread(0);
        component.set_sidebar_expanded(false);

        // Unlike Ctrl+Alt+Up/Down and Ctrl+K, Ctrl+B/N/, aren't in C++'s
        // `isThreadCommandChord` bypass list, so `panel_rust_input_key`'s
        // own focus guard applies: an editable Slint surface must already
        // own focus or the key never reaches Slint at all. Focus the
        // composer first, exactly as the app's own click-to-focus path
        // does, before sending the chord.
        let compose = ElementHandle::find_by_accessible_label(&component, "Compose message")
            .next()
            .expect("compose input must be accessible");
        let position = compose.absolute_position();
        let size = compose.size();
        assert!(panel_rust_input_click(
            panel.handle,
            (position.x + size.width / 2.0) as c_uint,
            (position.y + size.height / 2.0) as c_uint,
        ));
        assert!(component.get_compose_has_focus());

        panel.press_only(QT_KEY_CONTROL);
        panel.press_and_release(QT_KEY_B, "\u{2}");
        panel.release_only(QT_KEY_CONTROL);

        assert!(
            component.get_sidebar_expanded(),
            "Ctrl+B's real control-character text (\\u{{2}}) must still toggle the sidebar \
             once map_qt_key recovers the plain letter"
        );
    }
}
