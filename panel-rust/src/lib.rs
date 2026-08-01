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
mod markdown_worker;
mod model;
pub mod models;
mod msg;
mod permission;
pub mod project_store;
pub mod protocol_types;
mod send_queue;
mod settings_file;
pub mod snapflow_session_client;
pub mod snapshotd_client;
// `pub` (not just `mod`) so the `snapflowd-mcp` bin target can
// reuse `scan_skills_dir`/`global_skills_dir`/`project_skills_dir` instead
// of duplicating the SKILL.md front-matter parsing logic.
mod skills_manager_adapter;
pub mod skills_state;
mod snapshotd_lifecycle;
mod state_store;
mod sync;
mod thread_view;
// `pub` (not just `mod`) so `tests/*.rs` integration tests -- separate
// crates from this one, unable to see anything less than `pub` -- can
// reuse `agent_bridge`'s TOCTOU-safe ephemeral-port reservation instead
// of each keeping its own unsynchronized `free_port()` copy. Found live
// (worktree-project-isolation's own test-flakiness investigation,
// 2026-07-25): `free_port()` was duplicated into five separate
// `tests/*.rs` e2e harnesses, each with the same bind-then-drop-then-
// hope-nobody-else-grabs-it gap that `agent_bridge.rs`'s own unit tests
// used to have before switching onto `reserve_ephemeral_port`'s
// lock-file convention -- see that function's doc comment for the full
// root-cause writeup. `agent_bridge` itself stays `mod` (private): only
// this narrow reservation helper is meant to be public, not its whole
// internal surface.
pub mod test_support {
    pub use crate::agent_bridge::{reserve_ephemeral_port, reserve_port};
    // PISO-8 (project-isolation-mlt-binding plan): lets a real e2e test
    // (a separate crate, same `pub`-only visibility rule as above) drive
    // the actual `snapshotd list`/`listProjects` subprocess round trip
    // against a real spawned daemon, rather than only exercising the
    // pure JSONL-parsing helper via `agent_bridge`'s own unit tests.
    pub use crate::agent_bridge::{fetch_daemon_project_instances, DaemonProjectInstance};
}
mod theme;
mod thread_message_index;
mod update;

use agent_bridge::{resolve_cache_dir, AgentBridge, ThreadSpec, NO_PROVIDER_REQUESTED_FALLBACK};
use appearance::{ColorScheme, HostAppearance};
use models::ThreadState;
use protocol_types::{ChatMessage, MessageKind};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{
    EventLoopProxy, Key, Platform, PointerEventButton, WindowAdapter, WindowEvent,
};
use slint::{SharedString, VecModel};
use state_store::{PanelDefaults, PanelStateStore, SessionDerivedState};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::os::raw::{c_int, c_uchar, c_uint};
use std::rc::Rc;
use std::sync::Arc;

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

/// PROF-1/PROF-2: builds the cold-start `ThreadSpec`s for a fresh install
/// (`restored_records.is_empty()`) -- pulled out of the giant window-setup
/// function purely so this rule is unit-testable on its own.
///
/// `configured_agent_id` is this machine's real, non-sentinel
/// `default_agent_id` from resolved settings, if any (settings.global.json
/// / bundled defaults, the same value the settings sheet's agent picker
/// writes) -- `None` means genuinely nothing is configured yet.
///
/// Every seed thread shares ONE real provider: `configured_agent_id` when
/// set, else the single documented [`NO_PROVIDER_REQUESTED_FALLBACK`] --
/// never an index-parity guess alternating "codex"/"claude" by thread
/// position, which silently mis-bound half of every fresh install's
/// default threads to whichever provider parity happened to land on
/// regardless of what was actually configured/available.
///
/// `profile_name` (PROF-2) is bound to `configured_agent_id` too, but
/// ONLY when it's real -- never to the `NO_PROVIDER_REQUESTED_FALLBACK`
/// label, which is a bare gateway-routing key, not a real registry agent
/// id. acpx's `Router::ensure_default_profiles_seeded` auto-fills exactly
/// one profile per installed registry agent, named after that agent's own
/// id, so this name resolves without requiring any `profiles/create`
/// setup first (see `ProfileSource`'s own doc comment in
/// acpx-core/src/profile.rs) -- without this, a thread with a real
/// `default_agent_id` configured but no hand-picked profile silently fell
/// all the way through to acpx-server's own native/unmanaged-mode default
/// backend instead of the agent actually configured. Passing the bare
/// fallback label as `_acpx.profile` would instead make every
/// unconfigured-fresh-install `session/new` fail outright
/// (`RouterError::UnknownProfile`) -- strictly worse than today's
/// graceful native/unmanaged degrade -- so with nothing configured at
/// all, `profile_name` stays `None`.
fn cold_start_thread_specs(
    seed_names: &[&str],
    configured_agent_id: Option<String>,
) -> Vec<ThreadSpec> {
    let seed_provider = configured_agent_id
        .clone()
        .unwrap_or_else(|| NO_PROVIDER_REQUESTED_FALLBACK.to_owned());
    seed_names
        .iter()
        .map(|name| ThreadSpec {
            display_name: (*name).to_owned(),
            provider: seed_provider.clone(),
            session_id: None,
            profile_name: configured_agent_id.clone(),
            // Cold-start seed threads: nothing persisted yet, so there is
            // no stored association to hydrate from (PISO-3).
            project_path: None,
        })
        .collect()
}

#[cfg(test)]
mod cold_start_thread_specs_tests {
    use super::cold_start_thread_specs;

    /// PROF-2's own acceptance question: on a clean cache dir with no
    /// `default_agent_id` configured (the real "nothing set anywhere"
    /// case `cold_start_thread_specs` is called with when
    /// `settings_file::SettingsPaths::load_resolved` finds nothing), the
    /// seed threads get the one documented routing fallback for
    /// `provider` and stay unprofiled (`profile_name: None`) rather than
    /// guessing a profile name that would make `session/new` fail
    /// outright. This is an honest degrade to native/unmanaged mode, not
    /// a "usable default profile" -- see the written PROF-2 answer this
    /// test backs up.
    #[test]
    fn with_nothing_configured_seed_threads_use_the_fallback_provider_and_stay_unprofiled() {
        let specs = cold_start_thread_specs(&["Fix timeline crash", "Add fade transition"], None);
        assert_eq!(specs.len(), 2);
        for spec in &specs {
            assert_eq!(spec.provider, super::NO_PROVIDER_REQUESTED_FALLBACK);
            assert_eq!(spec.profile_name, None, "got: {specs:?}");
        }
    }

    /// With a real, configured `default_agent_id` (settings.global.json /
    /// bundled defaults), the first thread genuinely binds to a usable
    /// profile: `provider` AND `profile_name` both carry the real agent
    /// id, so `session/new`'s `_acpx.profile` resolves against acpx's own
    /// `ensure_default_profiles_seeded` auto-fill with zero
    /// `profiles/create` setup required.
    #[test]
    fn with_a_configured_default_agent_id_every_seed_thread_binds_to_it_as_its_profile() {
        let specs = cold_start_thread_specs(
            &[
                "Fix timeline crash",
                "Add fade transition",
                "Refactor filters",
            ],
            Some("codex-acp".to_owned()),
        );
        assert_eq!(specs.len(), 3);
        for spec in &specs {
            assert_eq!(spec.provider, "codex-acp");
            assert_eq!(
                spec.profile_name.as_deref(),
                Some("codex-acp"),
                "got: {specs:?}"
            );
        }
    }
}

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
fn maybe_migrate_sqlite_defaults_to_json(store: &PanelStateStore, warnings: &mut Vec<String>) {
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
        show_global_skills: None,
        harness: None,
        dev_mode: None,
        snapflow_mcp_enabled: None,
    };
    if let Err(error) = settings_file::save_document(&paths.global, &doc) {
        let message = format!("failed to migrate panel defaults to JSON: {error}");
        eprintln!("panel-rust: {message}");
        warnings.push(message);
    }
}

/// Load multi-process panel prefs from JSON (project → global → default).
/// `selected_thread_id` remains process-local (SQLite) when provided.
fn load_panel_prefs(
    selected_thread_id: Option<String>,
    warnings: &mut Vec<String>,
) -> PanelDefaults {
    let paths = settings_file::SettingsPaths::from_env();
    match paths.load_resolved() {
        Ok(resolved) => settings_file::resolved_to_panel_defaults(&resolved, selected_thread_id),
        Err(error) => {
            let message = format!("settings file load failed: {error}");
            eprintln!("panel-rust: {message}");
            warnings.push(message);
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
    show_global_skills: bool,
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
    warnings: &mut Vec<String>,
) -> Option<ScopedPanelPrefs> {
    let paths = settings_file::SettingsPaths::from_env();
    if scoped_settings_path(&paths, scope).is_none() {
        let message = format!("unavailable settings scope {scope:?}");
        eprintln!("panel-rust: {message}");
        warnings.push(message);
        return None;
    }

    let mut documents = Vec::new();
    if let Some(path) = paths.bundled_default.as_deref() {
        match settings_file::load_document(path) {
            Ok(document) => documents.push(document),
            Err(error) => {
                let message = format!("bundled settings load failed: {error}");
                eprintln!("panel-rust: {message}");
                warnings.push(message);
                return None;
            }
        }
    }
    match settings_file::load_document(&paths.global) {
        Ok(document) => documents.push(document),
        Err(error) => {
            let message = format!("global settings load failed: {error}");
            eprintln!("panel-rust: {message}");
            warnings.push(message);
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
                let message = format!("project settings load failed: {error}");
                eprintln!("panel-rust: {message}");
                warnings.push(message);
                return None;
            }
        }
    }

    let refs: Vec<&settings_file::SettingsDocument> = documents.iter().collect();
    let resolved = settings_file::merge_documents(&refs);
    Some(ScopedPanelPrefs {
        defaults: settings_file::resolved_to_panel_defaults(&resolved, selected_thread_id),
        default_agent_id: resolved.default_agent_id,
        show_global_skills: resolved.show_global_skills,
    })
}

/// Persist profile / permission / background-default / default-agent /
/// show-global-skills into the selected JSON tier. Existing unrelated
/// fields (harness, dev mode, ...) are retained by the read-modify-write
/// operation.
fn save_panel_prefs_to_json(
    scope: &str,
    defaults: &PanelDefaults,
    default_agent_id: Option<String>,
    show_global_skills: bool,
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
    doc.show_global_skills = Some(show_global_skills);
    settings_file::save_document(path, &doc).map_err(|error| error.to_string())
}

/// Feature-flag gate for the "default profile"/"permission profile"
/// Settings controls (`agents_view.slint`). Both are genuinely dual-tier
/// (visible under Project and Global scope alike -- unlike the six
/// categories gated Global-only in 6745aa0e), but until this is explicitly
/// turned on they're hidden entirely, in both scopes. Defaults OFF (unset
/// env var => hidden) so an unset environment never surfaces them.
fn profile_wiring_enabled() -> bool {
    std::env::var("PANEL_PROFILE_WIRING_ENABLED")
        .map(|value| value == "1")
        .unwrap_or(false)
}

/// Verifies the Project-vs-Global tiering that `HarnessView`'s
/// "Background sessions" toggle (`background_session_default`) is
/// supposed to have, end to end through the exact functions the UI save
/// path calls (`save_panel_prefs_to_json`/`load_scoped_panel_prefs`/
/// `scoped_settings_path`) rather than re-testing `settings_file.rs`'s
/// already-covered `merge_documents` in isolation. Mirrors
/// `settings_file.rs`'s own env-driven `SettingsPaths::from_env` tests,
/// and the save/restore-env-var shape `lifecycle_tests::
/// panel_create_destroy_create_reuses_slint_platform` already uses in this
/// file for other `RUI_*`-driven state.
#[cfg(test)]
mod scoped_panel_prefs_tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// `SettingsPaths::from_env` reads process-wide env vars, and Rust
    /// runs `#[test]` functions in parallel by default -- without this,
    /// this module's two tests race on `RUI_PANEL_SETTINGS_DIR`/
    /// `RUI_PANEL_PROJECT_ROOT` and spuriously observe each other's
    /// tempdirs. Serialize them the same way any env-var-mutating test
    /// suite must.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// RAII guard: snapshots and restores the handful of `RUI_PANEL_*` env
    /// vars `SettingsPaths::from_env` reads, so this test can't leak state
    /// into any test that runs after it in the same process. Holds the
    /// serialization lock for its whole lifetime.
    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let lock = env_lock();
            let keys = [
                "RUI_PANEL_SETTINGS_DIR",
                "RUI_ACP_CACHE_DIR",
                "RUI_PANEL_PROJECT_ROOT",
                "RUI_PANEL_SETTINGS_DEFAULT",
            ];
            let saved = keys
                .iter()
                .map(|&key| (key, std::env::var_os(key)))
                .collect();
            Self {
                _lock: lock,
                saved,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn defaults_with_background(background_session: bool) -> PanelDefaults {
        PanelDefaults {
            profile_name: None,
            permission_profile: None,
            background_session,
            selected_thread_id: None,
        }
    }

    /// Setting `background_session_default` under Project scope must land
    /// only in the project JSON file, leaving a separately-read Global
    /// value untouched -- and vice versa. This is the exact "set under one
    /// scope, confirm the other scope's separately-read value is
    /// unaffected" shape called for by the bug report, run through the
    /// real UI save/load functions instead of the lower-level
    /// `merge_documents` helper `settings_file.rs` already covers.
    #[test]
    fn background_session_default_persists_independently_per_scope() {
        let _guard = EnvGuard::new();
        let settings_dir = tempfile::tempdir().expect("settings dir");
        let project_root = tempfile::tempdir().expect("project root");
        std::env::set_var("RUI_PANEL_SETTINGS_DIR", settings_dir.path());
        std::env::set_var("RUI_PANEL_PROJECT_ROOT", project_root.path());
        std::env::remove_var("RUI_PANEL_SETTINGS_DEFAULT");
        std::env::remove_var("RUI_ACP_CACHE_DIR");

        let paths = settings_file::SettingsPaths::from_env();
        let global_path = paths.global.clone();
        let project_path = paths.project.clone().expect("project path resolved");
        assert_ne!(
            global_path, project_path,
            "project and global settings paths must never collide"
        );

        // Global starts true, Project starts unset (inherits Global).
        save_panel_prefs_to_json("global", &defaults_with_background(true), None, true)
            .expect("save global");
        let mut warnings = Vec::new();
        let global_prefs = load_scoped_panel_prefs("global", None, &mut warnings)
            .expect("load global");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert!(global_prefs.defaults.background_session);
        let project_prefs = load_scoped_panel_prefs("project", None, &mut warnings)
            .expect("load project (inherits global)");
        assert!(
            project_prefs.defaults.background_session,
            "project scope must inherit the global value when it has no override"
        );

        // Now set Project to false: this must only touch the project
        // file, and Global's own separately-read value must stay true.
        save_panel_prefs_to_json("project", &defaults_with_background(false), None, true)
            .expect("save project");
        let project_prefs = load_scoped_panel_prefs("project", None, &mut warnings)
            .expect("load project after override");
        assert!(
            !project_prefs.defaults.background_session,
            "project override must take effect for project scope"
        );
        let global_prefs = load_scoped_panel_prefs("global", None, &mut warnings)
            .expect("load global after project write");
        assert!(
            global_prefs.defaults.background_session,
            "a project-scope write must not affect the separately-read global value"
        );

        // And the raw files on disk confirm the write actually targeted
        // separate paths, not the same one under two names.
        let global_doc = settings_file::load_document(&global_path).expect("read global doc");
        let project_doc = settings_file::load_document(&project_path).expect("read project doc");
        assert_eq!(global_doc.background_session_default, Some(true));
        assert_eq!(project_doc.background_session_default, Some(false));
    }

    /// Same shape, opposite direction: a Global-scope write after a
    /// Project override exists must not clobber or be read back through
    /// the Project file.
    #[test]
    fn global_write_does_not_affect_an_existing_project_override() {
        let _guard = EnvGuard::new();
        let settings_dir = tempfile::tempdir().expect("settings dir");
        let project_root = tempfile::tempdir().expect("project root");
        std::env::set_var("RUI_PANEL_SETTINGS_DIR", settings_dir.path());
        std::env::set_var("RUI_PANEL_PROJECT_ROOT", project_root.path());
        std::env::remove_var("RUI_PANEL_SETTINGS_DEFAULT");
        std::env::remove_var("RUI_ACP_CACHE_DIR");

        save_panel_prefs_to_json("project", &defaults_with_background(true), None, true)
            .expect("save project override");
        save_panel_prefs_to_json("global", &defaults_with_background(false), None, true)
            .expect("save global default");

        let mut warnings = Vec::new();
        let project_prefs = load_scoped_panel_prefs("project", None, &mut warnings)
            .expect("load project");
        assert!(
            project_prefs.defaults.background_session,
            "existing project override must survive an unrelated global write"
        );
        let global_prefs = load_scoped_panel_prefs("global", None, &mut warnings)
            .expect("load global");
        assert!(!global_prefs.defaults.background_session);
    }

    /// Same end-to-end shape as `background_session_default_persists_
    /// independently_per_scope`, for `show_global_skills` -- the other
    /// setting named in the bug report (`skills_view.slint`'s "Show
    /// global skills" toggle). Before this fix it was pure component-local
    /// Slint UI state that never called into `save_panel_prefs_to_json`/
    /// `load_scoped_panel_prefs` at all, so it could never have collided
    /// on a shared file -- but it also never round-tripped through
    /// Project/Global scope like the bug report expected. This proves the
    /// newly-added wiring gives it the exact same per-scope isolation
    /// `background_session_default` already has.
    #[test]
    fn show_global_skills_persists_independently_per_scope() {
        let _guard = EnvGuard::new();
        let settings_dir = tempfile::tempdir().expect("settings dir");
        let project_root = tempfile::tempdir().expect("project root");
        std::env::set_var("RUI_PANEL_SETTINGS_DIR", settings_dir.path());
        std::env::set_var("RUI_PANEL_PROJECT_ROOT", project_root.path());
        std::env::remove_var("RUI_PANEL_SETTINGS_DEFAULT");
        std::env::remove_var("RUI_ACP_CACHE_DIR");

        // Global starts true, Project starts unset (inherits Global).
        save_panel_prefs_to_json("global", &defaults_with_background(false), None, true)
            .expect("save global");
        let mut warnings = Vec::new();
        let global_prefs = load_scoped_panel_prefs("global", None, &mut warnings)
            .expect("load global");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert!(global_prefs.show_global_skills);
        let project_prefs = load_scoped_panel_prefs("project", None, &mut warnings)
            .expect("load project (inherits global)");
        assert!(
            project_prefs.show_global_skills,
            "project scope must inherit the global value when it has no override"
        );

        // Set Project to false: must only touch the project file: Global's
        // own separately-read value must stay true.
        save_panel_prefs_to_json("project", &defaults_with_background(false), None, false)
            .expect("save project");
        let project_prefs = load_scoped_panel_prefs("project", None, &mut warnings)
            .expect("load project after override");
        assert!(
            !project_prefs.show_global_skills,
            "project override must take effect for project scope"
        );
        let global_prefs = load_scoped_panel_prefs("global", None, &mut warnings)
            .expect("load global after project write");
        assert!(
            global_prefs.show_global_skills,
            "a project-scope write must not affect the separately-read global value"
        );
    }
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
    // Set once at construction, true for the very first `SpikePlatform`
    // ever created in this process, false for every one after it. See
    // `new_event_loop_proxy`'s doc comment for why this must be a
    // per-*instance* flag, not a per-*call* one.
    is_first_platform: bool,
}

impl Platform for SpikePlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }

    // `runtime_gate_full_matrix`: without an event-loop proxy, any
    // `slint::spawn_local`/`Context::spawn_local` call (e.g. the one
    // `i_slint_backend_testing::mcp_server`'s window-shown hook makes to
    // launch its HTTP listener) fails immediately with
    // `EventLoopError::NoEventLoopProvider` -- the default `Platform`
    // impl returns `None` here. `SpikeEventLoopProxy` below queues the
    // closure and `panel_rust_poll` (already called every tick to drive
    // animations, see its own doc comment) drains it, so spawned local
    // futures actually make progress on this crate's one Rust thread
    // without needing Slint's own `run_event_loop()`.
    //
    // Must return `Some` for every call from the first-ever `SpikePlatform`
    // instance (real production has exactly one, for its whole lifetime,
    // and `Context::spawn_local` calls this fresh on every single
    // `spawn_local` invocation -- see `i-slint-core-1.17.1/future.rs`'s
    // `spawn_local_with_ctx`, which never caches the result), but `None`
    // for every call from any *later* instance: `slint::platform::
    // set_platform` stores its own `SlintContext` in a `thread_local!`
    // (fine -- a fresh one per test thread, since `TestPanel::new()`
    // constructs a whole new `SpikePlatform` per libtest-spawned test
    // thread), but it also forwards this return value into
    // `EVENTLOOP_PROXY`, a `static OnceCell` at the Slint-core level that
    // is genuinely process-global, not per-thread. Returning `Some` from
    // a second instance's `set_platform` call makes that `OnceCell::set`
    // fail, which `set_platform` surfaces as `SetPlatformError::
    // AlreadySet` for that *whole* call -- indistinguishable from (and
    // easily misread as) the platform itself already being set, but
    // really just this one proxy-registration cell colliding. Only the
    // first-ever instance can win that registration anyway, and only
    // that instance is ever the one a real `SLINT_MCP_PORT`-driven
    // process needs a working proxy for (every other instance only
    // exists inside this crate's own test suite, which never sets
    // `SLINT_MCP_PORT`), so declining unconditionally for every later
    // instance loses no functionality.
    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        if !self.is_first_platform {
            return None;
        }
        Some(Box::new(SpikeEventLoopProxy))
    }

    // Select-and-copy fix: the default `Platform::set_clipboard_text` is a
    // no-op, so a real user selection's Ctrl+C inside any `TextInput`
    // (compose box, read-only message text, terminal) silently went
    // nowhere -- `i-slint-core`'s `TextInput::copy_clipboard` calls this
    // hook directly and has no other path to a real OS clipboard. Delegate
    // to the SAME `write_clipboard_text` helper the existing whole-message
    // copy buttons already use via `Effect::ClipboardWrite`
    // (`effect_executor.rs`), so both paths land on the real system
    // clipboard through the identical wl-copy/xclip/xsel fallback chain.
    // Spawned on its own thread rather than called inline: it shells out
    // to a subprocess and waits for it, and this bridge's key-event
    // dispatch runs on the single UI thread (see this module's own
    // "this crate's one Rust thread" doc comments) -- blocking that
    // thread on a subprocess round trip for every Ctrl+C would be a real,
    // if usually small, UI stall. Not clipboard-kind-aware (routes both
    // `Clipboard::DefaultClipboard` -- Ctrl+C/Ctrl+V -- and
    // `Clipboard::SelectionClipboard` -- X11 primary-selection
    // auto-copy-on-select -- through the same system clipboard): this
    // platform only has one real clipboard integration, and landing a
    // primary-selection copy in the real clipboard too is strictly better
    // than the previous silent no-op.
    fn set_clipboard_text(&self, text: &str, _clipboard: slint::platform::Clipboard) {
        // Test-only observation hook: this environment has no real X11/
        // Wayland display (confirmed: `xclip`/`wl-copy`/`xsel` all fail
        // with "can't open display" headlessly here), so a test cannot
        // honestly assert against the real system clipboard. Recording
        // the exact string synchronously, on the same thread this method
        // is called on (before the real write is handed off to its own
        // thread below), lets a test prove the full real path -- a real
        // Ctrl+C key event, dispatched through the actual `panel_rust_
        // input_key` FFI bridge, on a real Slint text selection -- reaches
        // this platform hook with exactly the selected substring, without
        // needing a working clipboard helper to observe it.
        #[cfg(test)]
        {
            *LAST_CLIPBOARD_WRITE_FOR_TEST
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(text.to_owned());
        }
        let text = text.to_owned();
        std::thread::spawn(move || {
            let _ = effect_executor::write_clipboard_text(&text);
        });
    }

    fn clipboard_text(&self, _clipboard: slint::platform::Clipboard) -> Option<String> {
        effect_executor::read_clipboard_text()
    }
}

#[cfg(test)]
static LAST_CLIPBOARD_WRITE_FOR_TEST: std::sync::Mutex<Option<String>> =
    std::sync::Mutex::new(None);

static EVENT_LOOP_QUEUE: std::sync::Mutex<Vec<Box<dyn FnOnce() + Send>>> =
    std::sync::Mutex::new(Vec::new());

struct SpikeEventLoopProxy;

impl EventLoopProxy for SpikeEventLoopProxy {
    fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
        // panel-rust never runs Slint's own event loop (see module docs),
        // so there is nothing to quit.
        Ok(())
    }

    fn invoke_from_event_loop(
        &self,
        event: Box<dyn FnOnce() + Send>,
    ) -> Result<(), slint::EventLoopError> {
        EVENT_LOOP_QUEUE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
        Ok(())
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
    // Arc, not a plain owned value: several Effect handlers in
    // effect_executor.rs move a clone of this into a std::thread::spawn
    // closure so the blocking SQLite write happens off the Slint UI
    // thread (offload_state_effects_off_ui_thread).
    panel_state: Option<Arc<PanelStateStore>>,
    /// Identity-scoped durable stores. The legacy `panel_state` above is the
    /// global/None store and remains the migration fallback only.
    project_state_stores: RefCell<HashMap<model::ProjectIdentity, Arc<PanelStateStore>>>,
    /// Set by [`settings_file::SettingsWatcher`]; drained on poll to
    /// refresh open settings fields without clobbering dirty edits.
    settings_reload_pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Suppress self-write feedback from settings save for a short window.
    settings_ignore_watch_until: Cell<Option<std::time::Instant>>,
    _settings_watcher: Option<settings_file::SettingsWatcher>,
    snapshotd_registration: Option<snapshotd_client::SnapshotdRegistration>,
    /// markdown-render-cache-layer plan, Phase 7 trigger-wiring: shared
    /// across every thread's background markdown render (unlike
    /// `ThreadModel::markdown_epoch`/`markdown_in_flight`, which are
    /// deliberately per-thread) -- a fixed-size worker pool has no
    /// notion of which thread a job belongs to, so nothing about sharing
    /// it risks cross-thread interference the way a shared epoch counter
    /// would. Lives on `PanelSingleton`, not `Model`: it spawns real OS
    /// threads, which is infrastructure this crate's own TEA doc comment
    /// (see `model.rs`'s module doc) reserves for outside the pure
    /// reducer, alongside `bridge`/`panel_state`.
    markdown_render_pool: markdown_worker::RenderWorkerPool,
    /// Reconnecting daemon-side session metadata subscription. Kept behind a
    /// RefCell because endpoint discovery may become available after panel
    /// construction when snapflowd starts asynchronously.
    session_subscription: RefCell<Option<snapflow_session_client::SessionSubscription>>,
    session_cache_updates: Arc<std::sync::Mutex<VecDeque<snapflow_session_client::SessionUpdate>>>,
    session_cache_scope: RefCell<Option<model::ProjectIdentity>>,
    session_cache_hydrated: Cell<bool>,
}

impl PanelSingleton {
    fn project_identity_for_path(path: &str) -> model::ProjectIdentity {
        if path.ends_with(".mlt") {
            model::ProjectIdentity::Saved(path.to_owned())
        } else {
            model::ProjectIdentity::Untitled(path.to_owned())
        }
    }

    pub(crate) fn project_state_for_identity(
        &self,
        identity: &model::ProjectIdentity,
    ) -> Option<Arc<PanelStateStore>> {
        self.project_state_stores.borrow().get(identity).cloned()
    }

    /// Return the physically isolated durable store for the lifecycle
    /// identity currently folded into the model.
    pub(crate) fn active_panel_state(&self) -> Option<Arc<PanelStateStore>> {
        let identity = self.model.borrow().active_project.clone();
        if matches!(identity, model::ProjectIdentity::None) {
            return self.panel_state.clone();
        }
        if let Some(store) = self.project_state_stores.borrow().get(&identity).cloned() {
            return Some(store);
        }
        let path = crate::project_store::panel_state_path(&identity, &resolve_cache_dir());
        match PanelStateStore::open(path) {
            Ok(store) => {
                let store = Arc::new(store);
                self.project_state_stores
                    .borrow_mut()
                    .insert(identity, store.clone());
                Some(store)
            }
            Err(error) => {
                eprintln!("panel-rust: project state persistence unavailable: {error}");
                None
            }
        }
    }

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
        let Some(store) = self.active_panel_state() else {
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
            let message = format!("failed to synchronize runtime panel defaults: {error}");
            eprintln!("panel-rust: {message}");
            let _ = dispatch::update_persistent(
                self,
                msg::Msg::Effect(effect::EffectResultMsg::StateEffectFailed {
                    thread_id: String::new(),
                    message,
                }),
            );
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
        if let Some(error) = bridge.attachment_error(real_idx) {
            let _ = crate::dispatch::update_persistent(
                self,
                msg::Msg::Effect(effect::EffectResultMsg::PromptSent {
                    real_index: real_idx,
                    result: Err(effect::EffectError::new(format!(
                        "session attachment failed: {error}"
                    ))),
                }),
            );
            return;
        }
        self.start_send_prompt(real_idx, text, bridge);
    }

    fn start_send_prompt(&self, idx: usize, text: &str, bridge: &AgentBridge) {
        if bridge.thread_closed(idx) {
            trace_host_input(format_args!(
                "send ignored real_thread={idx} because the thread is closed"
            ));
            return;
        }
        if let Some(error) = bridge.attachment_error(idx) {
            let _ = crate::dispatch::update_persistent(
                self,
                msg::Msg::Effect(effect::EffectResultMsg::PromptSent {
                    real_index: idx,
                    result: Err(effect::EffectError::new(format!(
                        "session attachment failed: {error}"
                    ))),
                }),
            );
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

    /// Executes the bridge side of `Effect::KillAgentTerminal` (PUI-002b).
    pub(crate) fn execute_kill_agent_terminal_real(&self, real_idx: usize, terminal_id: String) {
        if let Some(bridge) = self.bridge.as_ref() {
            bridge.kill_terminal(real_idx, terminal_id);
        }
    }

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
            input.show_global_skills,
        ) {
            return Err(effect::EffectError::new(format!(
                "failed to save panel settings JSON: {error}"
            )));
        }
        let mut warnings = Vec::new();
        self.sync_runtime_defaults(&load_panel_prefs(None, &mut warnings));
        for message in warnings {
            let _ = dispatch::update_persistent(
                self,
                msg::Msg::Effect(effect::EffectResultMsg::StateEffectFailed {
                    thread_id: String::new(),
                    message,
                }),
            );
        }
        self.settings_ignore_watch_until.set(Some(
            std::time::Instant::now() + std::time::Duration::from_millis(500),
        ));
        if let Some(store) = self.active_panel_state().as_ref() {
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

    fn dispatch_mcp_server_tool_preference_changed_async(
        &self,
        server_name: &str,
        tool_name: &str,
        field: &str,
        value: bool,
        action_past_tense: &str,
    ) {
        let Some(bridge) = &self.bridge else {
            crate::effect_executor::report_mcp_server_result(Err(
                "no gateway connection".to_string(),
            ));
            return;
        };
        let gw = self.settings_gateway_index();
        let server_name = server_name.to_string();
        let tool_name = tool_name.to_string();
        let action_past_tense = action_past_tense.to_string();
        let callback_server_name = server_name.clone();
        let callback_tool_name = tool_name.clone();
        let callback_action = action_past_tense.clone();
        bridge.update_mcp_tool_preference_async(
            gw,
            &server_name,
            &tool_name,
            field,
            value,
            move |result| {
                crate::effect_executor::report_mcp_server_result(
                    result
                        .map(|()| format!("Tool \"{callback_tool_name}\" {callback_action}"))
                        .map_err(|err| {
                            format!(
                                "Failed to update tool \"{callback_tool_name}\" on MCP server \"{callback_server_name}\": {err}"
                            )
                        }),
                );
            },
        );
    }

    /// Per-tool enable flag on one MCP server entry. Persists into the
    /// entry's opaque `tools` JSON array via `mcp_servers/update`.
    pub(crate) fn dispatch_mcp_server_tool_enabled_changed(
        &self,
        _component: &ChatPanel,
        server_name: &str,
        tool_name: &str,
        enabled: bool,
    ) {
        self.dispatch_mcp_server_tool_preference_changed_async(
            server_name,
            tool_name,
            "enabled",
            enabled,
            if enabled { "enabled" } else { "disabled" },
        )
    }

    /// Per-tool deferred (lazy-load) flag -- same persisted `tools` JSON
    /// array as the enabled toggle, different field.
    pub(crate) fn dispatch_mcp_server_tool_deferred_changed(
        &self,
        _component: &ChatPanel,
        server_name: &str,
        tool_name: &str,
        deferred: bool,
    ) {
        self.dispatch_mcp_server_tool_preference_changed_async(
            server_name,
            tool_name,
            "deferred",
            deferred,
            if deferred { "set to deferred" } else { "set to eager" },
        )
    }

    /// The seven `dispatch_mcp_server_*_async` methods below are the only
    /// live dispatchers for "Fetch tools"/"Refresh tools" and the other
    /// six real UI-reachable MCP settings actions (Add/Save, Remove,
    /// enable toggle, Connect, Disconnect) -- PUI-013-style fix for the
    /// reported "jittery lag" while toggling/acting on an MCP server: a
    /// prior synchronous generation of these dispatchers each ended in
    /// `AgentBridge::block_on`, which ran on the Slint UI callback thread
    /// and froze the whole panel for the RPC's duration. These call the
    /// matching `AgentBridge::*_async` method instead (kicks the RPC off
    /// on the bridge's own tokio runtime) and pass a completion closure
    /// that formats the same success/failure
    /// message text the synchronous versions produce, then feeds it
    /// through `effect_executor::report_mcp_server_result` -- the closure
    /// runs on the runtime thread, not the UI thread, but `report_mcp_
    /// server_result` already re-enters the event loop itself (`slint::
    /// invoke_from_event_loop`), same as every other background-thread
    /// completion in this codebase, so this is thread-safe without an
    /// extra hop back through this type.
    pub(crate) fn dispatch_mcp_server_create_async(
        &self,
        _component: &ChatPanel,
        entry: crate::protocol_types::McpServerEntry,
    ) {
        let Some(bridge) = &self.bridge else {
            crate::effect_executor::report_mcp_server_result(Err(
                "no gateway connection".to_string()
            ));
            return;
        };
        let gw = self.settings_gateway_index();
        let name = entry.name.clone();
        bridge.create_mcp_server_async(gw, entry, move |result| {
            crate::effect_executor::report_mcp_server_result(
                result
                    .map(|()| format!("MCP server \"{name}\" created"))
                    .map_err(|err| format!("Failed to create MCP server \"{name}\": {err}")),
            );
        });
    }

    pub(crate) fn dispatch_mcp_server_update_async(
        &self,
        _component: &ChatPanel,
        entry: crate::protocol_types::McpServerEntry,
    ) {
        let Some(bridge) = &self.bridge else {
            crate::effect_executor::report_mcp_server_result(Err(
                "no gateway connection".to_string()
            ));
            return;
        };
        let gw = self.settings_gateway_index();
        let name = entry.name.clone();
        bridge.update_mcp_server_async(gw, entry, move |result| {
            crate::effect_executor::report_mcp_server_result(
                result
                    .map(|()| format!("MCP server \"{name}\" updated"))
                    .map_err(|err| format!("Failed to update MCP server \"{name}\": {err}")),
            );
        });
    }

    pub(crate) fn dispatch_mcp_server_delete_async(&self, _component: &ChatPanel, name: &str) {
        let Some(bridge) = &self.bridge else {
            crate::effect_executor::report_mcp_server_result(Err(
                "no gateway connection".to_string()
            ));
            return;
        };
        let gw = self.settings_gateway_index();
        let name = name.to_string();
        let callback_name = name.clone();
        bridge.delete_mcp_server_async(gw, &name, move |result| {
            crate::effect_executor::report_mcp_server_result(
                result
                    .map(|()| format!("MCP server \"{callback_name}\" removed"))
                    .map_err(|err| {
                        format!("Failed to remove MCP server \"{callback_name}\": {err}")
                    }),
            );
        });
    }

    pub(crate) fn dispatch_mcp_server_enabled_changed_async(
        &self,
        _component: &ChatPanel,
        name: &str,
        enabled: bool,
    ) {
        // Built-in snapflow: panel preference + pool opener rewrite, not
        // acpx `mcp_servers/update` (no central registry row exists).
        if crate::agent_bridge::is_builtin_snapflow_mcp_name(name) {
            let paths = settings_file::SettingsPaths::from_env();
            if let Err(error) = paths.set_snapflow_mcp_enabled(enabled) {
                crate::effect_executor::report_mcp_server_result(Err(format!(
                    "Failed to persist snapflow MCP enabled={enabled}: {error}"
                )));
                return;
            }
            if let Some(bridge) = &self.bridge {
                bridge.set_builtin_snapflow_mcp_enabled(enabled);
            } else {
                crate::agent_bridge::set_snapflow_mcp_enabled_flag(enabled);
            }
            crate::effect_executor::report_mcp_server_result(Ok(format!(
                "MCP server \"snapflow\" {}",
                if enabled { "enabled" } else { "disabled" }
            )));
            return;
        }
        let Some(bridge) = &self.bridge else {
            crate::effect_executor::report_mcp_server_result(Err(
                "no gateway connection".to_string()
            ));
            return;
        };
        let gw = self.settings_gateway_index();
        let name = name.to_string();
        let callback_name = name.clone();
        bridge.set_mcp_server_enabled_async(gw, &name, enabled, move |result| {
            crate::effect_executor::report_mcp_server_result(
                result
                    .map(|()| {
                        format!(
                            "MCP server \"{callback_name}\" {}",
                            if enabled { "enabled" } else { "disabled" }
                        )
                    })
                    .map_err(|err| {
                        format!(
                            "Failed to update enabled state for MCP server \"{callback_name}\": {err}"
                        )
                    }),
            );
        });
    }

    /// Non-blocking Connect. `opener::open` (opening the returned
    /// authorization URL in the default browser) is a fast, fire-and-forget
    /// OS call, so it still runs synchronously inside the completion
    /// closure -- only the network round-trip that discovers/starts the
    /// OAuth flow moves off the UI thread.
    pub(crate) fn dispatch_mcp_server_authenticate_async(&self, _component: &ChatPanel, name: &str) {
        let Some(bridge) = &self.bridge else {
            crate::effect_executor::report_mcp_server_result(Err(
                "no gateway connection".to_string()
            ));
            return;
        };
        let gw = self.settings_gateway_index();
        let name = name.to_string();
        let callback_name = name.clone();
        bridge.authenticate_mcp_server_async(gw, &name, move |result| {
            let outcome = match result {
                Ok(authorization_url) => match opener::open(&authorization_url) {
                    Ok(()) => Ok(format!("Opened browser to connect \"{callback_name}\"")),
                    Err(error) => Err(format!(
                        "MCP server \"{callback_name}\": failed to open browser for OAuth: {error}"
                    )),
                },
                Err(err) => Err(format!(
                    "Failed to start OAuth flow for MCP server \"{callback_name}\": {err}"
                )),
            };
            crate::effect_executor::report_mcp_server_result(outcome);
        });
    }

    pub(crate) fn dispatch_mcp_server_logout_async(&self, _component: &ChatPanel, name: &str) {
        let Some(bridge) = &self.bridge else {
            crate::effect_executor::report_mcp_server_result(Err(
                "no gateway connection".to_string()
            ));
            return;
        };
        let gw = self.settings_gateway_index();
        let name = name.to_string();
        let callback_name = name.clone();
        bridge.logout_mcp_server_async(gw, &name, move |result| {
            crate::effect_executor::report_mcp_server_result(
                result
                    .map(|()| format!("Disconnected \"{callback_name}\""))
                    .map_err(|err| {
                        format!("Failed to disconnect MCP server \"{callback_name}\": {err}")
                    }),
            );
        });
    }

    pub(crate) fn dispatch_mcp_server_tools_fetch_async(
        &self,
        _component: &ChatPanel,
        server_name: &str,
    ) {
        let Some(bridge) = &self.bridge else {
            crate::effect_executor::report_mcp_server_result(Err(
                "no gateway connection".to_string()
            ));
            return;
        };
        let gw = self.settings_gateway_index();
        let server_name = server_name.to_string();
        let callback_name = server_name.clone();
        bridge.fetch_mcp_server_tools_async(gw, &server_name, move |result| {
            crate::effect_executor::report_mcp_server_result(
                result
                    .map(|()| format!("Fetching tools for \"{callback_name}\"..."))
                    .map_err(|err| {
                        format!("Failed to fetch tools for MCP server \"{callback_name}\": {err}")
                    }),
            );
        });
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
        let profile_name = name.to_string();
        bridge.create_profile_async(gw, entry, move |result| {
            crate::effect_executor::report_mcp_server_result(
                result
                    .map(|()| format!("Profile \"{profile_name}\" created"))
                    .map_err(|err| format!("Failed to create profile \"{profile_name}\": {err}")),
            );
        });
    }

    pub(crate) fn dispatch_profile_delete(&self, _component: &ChatPanel, name: &str) {
        let Some(bridge) = &self.bridge else { return };
        let gw = self.settings_gateway_index();
        let profile_name = name.to_string();
        bridge.delete_profile_async(gw, name, move |result| {
            crate::effect_executor::report_mcp_server_result(
                result
                    .map(|()| format!("Profile \"{profile_name}\" deleted"))
                    .map_err(|err| format!("Failed to delete profile \"{profile_name}\": {err}")),
            );
        });
    }

    pub(crate) fn dispatch_agent_install_requested(&self, _component: &ChatPanel, agent_id: &str) {
        let Some(bridge) = &self.bridge else { return };
        let gw = self.settings_gateway_index();
        // PUI-013: fire-and-forget so the agents/install round-trip does not
        // block the Slint UI thread (the Settings>Agents Install freeze).
        bridge.install_agent_async(gw, agent_id);
    }

    /// setup-followups plan, agent_settings_ordering_and_install_enable_
    /// flow: the real "install > enable" second step. `real_index`/
    /// `_component` are unused (mirrors `dispatch_agent_install_
    /// requested`'s own shape) -- enable/disable is admin-plane-wide,
    /// not per-gateway-slot, so `AgentBridge::set_agent_enabled` doesn't
    /// need an index at all.
    pub(crate) fn dispatch_agent_set_enabled(&self, agent_id: &str, enabled: bool) {
        let Some(bridge) = &self.bridge else { return };
        // PUI-013: fire-and-forget so the admin-plane enable/disable
        // round-trip does not block the Slint UI thread.
        bridge.set_agent_enabled_async(agent_id, enabled);
    }

    /// Opens a registry-provided official website from an agent card. Only
    /// web URLs are accepted here; registry metadata must never turn the
    /// card into a general local-file or command opener.
    pub(crate) fn dispatch_agent_website_clicked(&self, website: &str) {
        let website = website.trim();
        if website.starts_with("https://") || website.starts_with("http://") {
            open_md_link_target(website);
        } else if !website.is_empty() {
            eprintln!("panel-rust: refusing non-web agent website URL: {website}");
        }
    }

    pub(crate) fn dispatch_dev_mode_toggled(&self, enabled: bool) {
        let paths = settings_file::SettingsPaths::from_env();
        if let Err(error) = paths.set_dev_mode(enabled) {
            let message = format!("failed to persist dev mode: {error}");
            eprintln!("panel-rust: {message}");
            let _ = dispatch::update_persistent(
                self,
                msg::Msg::Effect(effect::EffectResultMsg::StateEffectFailed {
                    thread_id: String::new(),
                    message,
                }),
            );
        }
        if enabled {
            let global_dir = crate::skills_state::global_skills_dir(&resolve_cache_dir());
            if let Err(error) = crate::skills_state::ensure_bundled_global_skill(&global_dir) {
                let message = format!("failed to install bundled global skill: {error}");
                eprintln!("panel-rust: {message}");
                let _ = dispatch::update_persistent(
                    self,
                    msg::Msg::Effect(effect::EffectResultMsg::StateEffectFailed {
                        thread_id: String::new(),
                        message,
                    }),
                );
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
        let (reason, generation, identity) = {
            let model = self.model.borrow();
            (
                model.project_lifecycle_reason.clone(),
                model.project_generation,
                model.active_project.clone(),
            )
        };
        if let Some(bridge) = self.bridge.as_ref() {
            // project-close-session-teardown: release every thread session
            // still bound to the project that is about to stop being
            // active -- BEFORE `set_active_project_identity` below
            // overwrites the bridge's own record of which project that is.
            // "switched"/"closed"/"opened"/"created_untitled" all mean the
            // previously active project (if any) is being left for real;
            // "saved_as" is deliberately excluded (see `release_sessions_
            // for_current_project`'s doc comment) since a Save-As/first-
            // save relabels the SAME live project rather than leaving it,
            // and releasing there would sever conversations mid-turn for
            // no reason.
            if reason != "saved_as" {
                bridge.release_sessions_for_current_project();
            }
            bridge.set_active_project_identity(&identity);
            let default_agent_id = self.model.borrow().default_agent_id.clone();
            if !default_agent_id.trim().is_empty() {
                bridge.prewarm_default_agent(&default_agent_id, Some(&default_agent_id));
            }
        }
        if let Some(registration) = self.snapshotd_registration.as_ref() {
            registration.update(path.clone(), reason, generation);
        }
        // Session-derived project/status state is daemon-authoritative. The
        // existing project/thread persistence above remains local, while the
        // session snapshot cache is written only from authenticated daemon
        // updates so a local revision-0 write cannot mask a newer snapshot.
    }

    pub(crate) fn refresh_session_subscription(
        &self,
    ) -> Vec<snapflow_session_client::SessionUpdate> {
        let active_scope = self.model.borrow().active_project.clone();
        if self.session_cache_scope.borrow().as_ref() != Some(&active_scope) {
            *self.session_cache_scope.borrow_mut() = Some(active_scope);
            self.session_cache_hydrated.set(false);
        }
        if self.session_subscription.borrow().is_none() {
            if let Some(endpoint) = snapflow_session_client::SessionEndpoint::discover() {
                *self.session_subscription.borrow_mut() = Some(
                    snapflow_session_client::SessionSubscription::start(endpoint),
                );
            }
        }
        let session_ids: Vec<String> = self
            .model
            .borrow()
            .threads
            .iter()
            .filter_map(|thread| thread.session_id.clone())
            .collect();
        if !self.session_cache_hydrated.get() {
            if let Some(store) = self.active_panel_state() {
                let queue = self
                    .session_subscription
                    .borrow()
                    .as_ref()
                    .map(|subscription| subscription.updates_handle())
                    .unwrap_or_else(|| Arc::clone(&self.session_cache_updates));
                let cached = store.all_session_derived_states().ok();
                if let Some(cached) = cached {
                    let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
                    for state in cached {
                        queue.push_back(snapflow_session_client::SessionUpdate {
                            client_instance_id: None,
                            snapshot: snapflow_session_client::SessionSnapshot {
                                session_id: state.session_id,
                                acp_session_id: state.acp_session_id,
                                project_id: state.project_id,
                                project_path: state.project_path,
                                connection_status: state.connection_status,
                                revision: state.revision,
                                created_at: String::new(),
                                expires_at: String::new(),
                            },
                        });
                        while queue.len() > 256 {
                            queue.pop_front();
                        }
                    }
                }
            }
            self.session_cache_hydrated.set(true);
        }
        if let Some(subscription) = self.session_subscription.borrow().as_ref() {
            subscription.set_sessions(session_ids);
        }
        let mut updates = self
            .session_cache_updates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect::<Vec<_>>();
        if let Some(subscription) = self.session_subscription.borrow().as_ref() {
            updates.extend(subscription.drain());
        }
        if let Some(store) = self.active_panel_state() {
            for update in &updates {
                let snapshot = &update.snapshot;
                let state = SessionDerivedState {
                    session_id: snapshot.session_id.clone(),
                    acp_session_id: snapshot.acp_session_id.clone(),
                    project_id: snapshot.project_id.clone(),
                    project_path: snapshot.project_path.clone(),
                    connection_status: snapshot.connection_status.clone(),
                    revision: snapshot.revision,
                };
                if let Err(error) = store.save_session_derived_state(&state) {
                    eprintln!("panel-rust: session-derived SQLite update failed: {error}");
                }
            }
        }
        updates
    }

    /// Move the durable project store during Save-As/first-save. The rename
    /// effect already owns this transition for live bridge associations; the
    /// filesystem half keeps the physical SQLite store aligned with it.
    pub(crate) fn move_project_store_for_rename(
        &self,
        old_identity: &model::ProjectIdentity,
        new: &str,
    ) {
        let cache_root = resolve_cache_dir();
        let new_identity = Self::project_identity_for_path(new);
        let old_dir = crate::project_store::project_store_dir(old_identity, &cache_root);
        let new_dir = crate::project_store::project_store_dir(&new_identity, &cache_root);
        let (Some(old_dir), Some(new_dir)) = (old_dir, new_dir) else {
            return;
        };
        if old_dir == new_dir || !old_dir.exists() {
            return;
        }
        if new_dir.exists() {
            eprintln!(
                "panel-rust: retaining existing project store at {} during rename from {}",
                new_dir.display(),
                old_dir.display()
            );
            return;
        }
        if let Some(parent) = new_dir.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                eprintln!("panel-rust: failed to create project store parent: {error}");
                return;
            }
        }
        if let Err(error) = std::fs::rename(&old_dir, &new_dir) {
            eprintln!(
                "panel-rust: failed to move project store {} -> {}: {error}",
                old_dir.display(),
                new_dir.display()
            );
        } else {
            let moved_store = self.project_state_stores.borrow_mut().remove(old_identity);
            if let Some(store) = moved_store {
                // The SQLite connection remains valid after the directory move,
                // but its registry key must move with the project identity or a
                // later switch can reopen the old path and leave two live cache
                // entries for the same physical store.
                self.project_state_stores
                    .borrow_mut()
                    .insert(new_identity, store);
            }
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
    panel_rust_create_with_initial_identity(width, height, None)
}

fn panel_rust_create_with_initial_identity(
    width: c_uint,
    height: c_uint,
    initial_identity: Option<model::ProjectIdentity>,
) -> *mut PanelHandle {
    let existing_handled = PANEL.with(|cell| {
        let mut slot = cell.borrow_mut();
        if let Some(existing) = slot.as_mut() {
            if existing.width != width || existing.height != height {
                existing
                    .window
                    .set_size(slint::PhysicalSize::new(width, height));
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
            true
        } else {
            false
        }
    });
    if existing_handled {
        return &SENTINEL as *const PanelHandle as *mut PanelHandle;
    }

        let window = PLATFORM_WINDOW.with(|platform_window| {
            let mut platform_window = platform_window.borrow_mut();
            if let Some(window) = platform_window.as_ref() {
                return window.clone();
            }
            let window = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
            static FIRST_PLATFORM_CLAIMED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            let is_first_platform =
                !FIRST_PLATFORM_CLAIMED.swap(true, std::sync::atomic::Ordering::SeqCst);
            let _ = slint::platform::set_platform(Box::new(SpikePlatform {
                window: window.clone(),
                is_first_platform,
            }));
            // See the `component.window().show()` call below (right after
            // `ChatPanel::new()`) for why that call, not this one, is what
            // actually makes the MCP server's HTTP listener start.
            //
            // Must run at most once per *process*, not once per thread:
            // `PLATFORM_WINDOW` above is `thread_local!`, so this branch is
            // re-entered on every fresh OS thread that calls
            // `panel_rust_create` for the first time on that thread --
            // harmless in production (one long-lived thread), but
            // `TestPanel::new()` in this crate's own test suite constructs
            // many panels, each on its own libtest-spawned thread. Calling
            // `mcp_server::init()` again on a second thread registers a
            // process-wide hook that holds the first thread's platform/
            // window state alive past that thread's exit, which then makes
            // *this exact* `set_platform` call above panic
            // (`AlreadySet`) for the *next* test thread instead of
            // succeeding the way it always did before this hook existed --
            // a real regression this `Once` guard closes without changing
            // the single-process production behavior at all.
            static MCP_SERVER_INIT: std::sync::Once = std::sync::Once::new();
            MCP_SERVER_INIT.call_once(|| {
                let _ = i_slint_backend_testing::mcp_server::init();
            });
            *platform_window = Some(window.clone());
            window
        });
        window.set_size(slint::PhysicalSize::new(width, height));

        let component = match ChatPanel::new() {
            Ok(c) => c,
            Err(_) => return std::ptr::null_mut(),
        };
        // `runtime_gate_full_matrix` (video-generation-e2e-harness plan):
        // panel-rust never runs Slint's own event loop (Qt drives rendering
        // via panel_rust_poll/panel_rust_render), so this is the only place
        // that ever calls the official `Window::show()` lifecycle method --
        // required because `i_slint_backend_testing::mcp_server::init()`'s
        // deferred HTTP-server spawn is gated on Slint core's internal
        // `window_shown_hook`, which only `Window::show()` fires. Calling it
        // here (once, on the real associated top-level component, right
        // after construction) is the officially supported public API for
        // this -- it does not start Slint's own event loop or otherwise
        // change how rendering is driven; see `Window::show()`'s own doc
        // comment (registers the window/component with the windowing
        // system, syncs geometry, and fires the hook -- all one-shot,
        // idempotent bookkeeping compatible with a manual render loop).
        let _ = component.window().show();
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
        // Cold-start failures collected here previously only reached
        // eprintln! -- folded into InitialState::startup_warnings below so
        // update()'s InitialStateLoaded handler can surface them as
        // Dirty::Error instead of silently degrading with no UI signal.
        let mut startup_warnings: Vec<String> = Vec::new();
        let panel_state = {
            let cache_dir = resolve_cache_dir();
            let identity = initial_identity
                .as_ref()
                .unwrap_or(&model::ProjectIdentity::None);
            let path = crate::project_store::panel_state_path(identity, &cache_dir);
            match PanelStateStore::open(path) {
                Ok(store) => Some(Arc::new(store)),
                Err(error) => {
                    let message = format!("panel settings persistence unavailable: {error}");
                    eprintln!("panel-rust: {message}");
                    startup_warnings.push(message);
                    None
                }
            }
        };
        let restored_records = panel_state
            .as_ref()
            .and_then(|store| match store.thread_records() {
                Ok(records) => Some(records),
                Err(error) => {
                    let message = format!("failed to restore chat thread records: {error}");
                    eprintln!("panel-rust: {message}");
                    startup_warnings.push(message);
                    None
                }
            })
            .unwrap_or_default();
        // Cold-start seed when panel-state has no prior threads.
        // PISO-13 (user report, 2026-07-25): "stale 4 threads bundled in
        // production ... we don't want these at all in production ...
        // user chooses the threads". Prior to this fix, an UNSET
        // RUI_SEED_THREADS defaulted to the full DEFAULT_THREAD_NAMES
        // fixture set on EVERY build, including a real production launch --
        // there was no signal at all that distinguished "a dev/QA harness
        // that wants demo content" from "a real user's first launch", so a
        // production install unconditionally got 4 threads named "Fix
        // timeline crash" etc. that nobody created and nobody asked for.
        // The prior comment here already correctly diagnosed the surface
        // symptom (these look like leftover work on any restore-empty
        // launch) but the prescribed fix -- "VNC harnesses must set
        // RUI_SEED_THREADS=0" -- was aspirational and never actually done
        // anywhere in this repo (grepped: zero scripts set it), so the
        // fixture set kept shipping to everyone by default regardless.
        // RUI_SEED_THREADS now:
        //   unset    -> single empty "Chat" (the real default: no fixture
        //                content, the user creates their own threads)
        //   "0"      -> same as unset, kept for any caller that already
        //                passes it explicitly
        //   "1".."N" -> first N of DEFAULT_THREAD_NAMES (capped) -- now an
        //                explicit OPT-IN for dev/QA/demo harnesses that
        //                genuinely want named fixture content, not
        //                something a real launch falls into by default.
        // A panel without a project identity must still restore durable
        // *unscoped* records from the global panel store. Only the synthetic
        // cold-start seed is suppressed in that state; otherwise restart
        // would erase the visible thread list until the user opened a
        // project. Restored records carry their own project_path, so this
        // does not invent a cwd or bind an old session to the host process.
        // bug3_always_have_a_default_thread: previously also required
        // `initial_identity.is_some()`, so a cold start with no project
        // open yet seeded zero threads ("No thread" empty state). The
        // bridge/gateway below is already constructed unconditionally;
        // seed the same default thread here too so one is always open.
        let mut initial_specs: Vec<ThreadSpec> = if restored_records.is_empty() {
            let seed_names: Vec<&str> = match std::env::var("RUI_SEED_THREADS") {
                Ok(v) if v.trim() == "0" => vec!["Chat"],
                Ok(v) => {
                    let n = v
                        .trim()
                        .parse::<usize>()
                        .unwrap_or(DEFAULT_THREAD_NAMES.len());
                    DEFAULT_THREAD_NAMES.iter().copied().take(n).collect()
                }
                Err(_) => vec!["Chat"],
            };
            let configured_agent_id = settings_file::SettingsPaths::from_env()
                .load_resolved()
                .ok()
                .and_then(|resolved| {
                    settings_file::non_default_sentinel(resolved.default_agent_id)
                });
            cold_start_thread_specs(&seed_names, configured_agent_id)
        } else {
            restored_records
                .iter()
                .map(|record| ThreadSpec {
                    display_name: record.display_name.clone(),
                    provider: record.provider.clone(),
                    session_id: Some(record.session_id.clone()),
                    // `update.rs`'s `ThreadMsg::New` and `settings_file.rs`'s
                    // `resolved_to_panel_defaults` both guard against the
                    // literal "default" sentinel (a reserved acpx-server
                    // placeholder, never a real profile name) at their own
                    // point of use/write -- but a thread record persisted
                    // to panel-state.sqlite3 *before* either fix landed (or
                    // written by some other path that predates them) still
                    // carries that literal string forward untouched on cold
                    // start, restoring straight into a real session/new
                    // call and silently misrouting that thread's agent
                    // forever ("the default thread's chat input isn't
                    // linked to a real agent"). Guard here too, at this
                    // third point of use.
                    profile_name: settings_file::non_default_sentinel(record.profile_name.clone()),
                    // PISO-3: hydrate the durable per-thread project
                    // association straight from the persisted record, so
                    // `AgentBridge::new_with_thread_specs` can bind the
                    // restored slot's `project_path` to what this thread
                    // was actually created under -- not whatever project
                    // happens to be active at this restart.
                    project_path: record.project_path.clone(),
                })
                .collect()
        };
        if let Some(identity) = initial_identity.as_ref() {
            // Fresh and legacy rows in this project-local store inherit the
            // current saved MLT association. Untitled remains pathless in
            // durable thread metadata, but its initial store cwd is passed
            // separately below.
            if let Some(saved_path) = identity.saved_path() {
                for spec in &mut initial_specs {
                    if spec.project_path.is_none() {
                        spec.project_path = Some(saved_path.to_owned());
                    }
                }
            }
        }
        let initial_permission_profiles: Vec<Option<String>> = restored_records
            .iter()
            .map(|record| settings_file::non_default_sentinel(record.permission_profile.clone()))
            .chain(std::iter::repeat(None))
            .take(initial_specs.len())
            .collect();
        // The bridge owns the panel-level ACPX server connection as well as
        // per-thread sessions. Construct it even before a project is open or
        // a chat thread exists, so Settings > Agents/MCP can discover the
        // live gateway on the empty-project screen. Project identity remains
        // optional input to the constructor; it only controls persistence
        // scope and session cwd.
        let initial_cwd = initial_identity.as_ref().and_then(|identity| {
            crate::project_store::project_store_dir(identity, &resolve_cache_dir())
        });
        // Seed built-in snapflow MCP injection from Global settings before
        // any pool/session is built so cold-start openers omit it when off.
        {
            let paths = settings_file::SettingsPaths::from_env();
            crate::agent_bridge::set_snapflow_mcp_enabled_flag(paths.snapflow_mcp_enabled());
        }
        let (bridge, bridge_available) =
            match AgentBridge::new_with_thread_specs_and_initial_identity(
                &initial_specs,
                initial_cwd,
                initial_identity
                    .as_ref()
                    .and_then(|identity| identity.saved_path().map(std::path::PathBuf::from)),
            ) {
                Ok(b) => (Some(b), true),
                Err(e) => {
                    let message =
                        format!("agent bridge unavailable, chat panel is display-only: {e}");
                    eprintln!("panel-rust: {message}");
                    startup_warnings.push(message);
                    (None, false)
                }
            };
        if let Some(bridge) = bridge.as_ref() {
            if let Some(store) = panel_state.as_ref() {
                for (index, record) in restored_records.iter().enumerate() {
                    if let Ok(background) = store.effective_background_session(&record.thread_id) {
                        bridge.set_thread_background(index, background);
                    }
                }
            }
        }
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
        // Cold-start payload is collected once, then folded only through
        // Init -> InitialStateLoaded. The shell Model starts empty so the
        // TEA path owns the first real hydration (no pre-seed + replace).
        let initial_thread_ids: Vec<String> = restored_records
            .iter()
            .map(|record| record.thread_id.clone())
            .chain((restored_records.len()..initial_specs.len()).map(|idx| format!("thread:{idx}")))
            .collect();
        // Each thread's send queue persists to its own
        // <thread_id>.sendqueue.jsonl (see send_queue.rs's module doc) --
        // restore it here (real disk I/O, so it belongs in this cold-start
        // collection step, never inside update()'s pure reducer). A
        // missing/corrupt file falls back to an empty queue still wired
        // to persist going forward, same posture as this function's other
        // cache reads.
        let cache_dir = resolve_cache_dir();
        let initial_send_queues: Vec<send_queue::SendQueue> = initial_thread_ids
            .iter()
            .map(|thread_id| {
                let path = send_queue::send_queue_path(&cache_dir, thread_id);
                send_queue::SendQueue::load(path.clone()).unwrap_or_else(|error| {
                    eprintln!(
                        "panel-rust: failed to restore send queue for thread {thread_id:?}: {error}"
                    );
                    send_queue::SendQueue::new_with_path(path)
                })
            })
            .collect();
        let initial = model::InitialState {
            threads: initial_specs.clone(),
            thread_ids: initial_thread_ids,
            selected_thread_id: initial_selected_thread_id.clone(),
            permission_profiles: initial_permission_profiles.clone(),
            send_queues: initial_send_queues,
            thread_states: if bridge_available {
                vec![ThreadState::Idle; initial_specs.len()]
            } else {
                vec![ThreadState::Error; initial_specs.len()]
            },
            startup_warnings,
        };
        let mut model = model::Model::default();
        model.profile_wiring_enabled = profile_wiring_enabled();
        let thread_model = Rc::new(VecModel::default());
        let skills_model = Rc::new(VecModel::default());
        model.thread_model = thread_model.clone();
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
            project_state_stores: RefCell::new(HashMap::new()),
            settings_reload_pending,
            settings_ignore_watch_until: Cell::new(None),
            _settings_watcher: settings_watcher,
            // Register the GUI process even when no project is open yet.  The
            // daemon owns the process lifecycle independently of project
            // selection; waiting for an initial identity made a fresh panel
            // invisible to snapshotd until the first project was opened.
            snapshotd_registration: snapshotd_client::SnapshotdRegistration::start(
                initial_identity
                    .as_ref()
                    .and_then(|identity| identity.saved_path().map(str::to_owned)),
            ),
            // Small fixed pool -- markdown rendering is not the only thing
            // competing for CPU, and this is background pre-render work,
            // not latency-critical (the synchronous path already shows
            // correct content the same frame; this pool exists to warm
            // the per-thread cache for a later switch, not to gate any
            // visible render).
            markdown_render_pool: markdown_worker::RenderWorkerPool::new(2),
            session_subscription: RefCell::new(None),
            session_cache_updates: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            session_cache_scope: RefCell::new(None),
            session_cache_hydrated: Cell::new(false),
        };
        // Gateway availability is panel-scoped, independent of project-open.
        // This enables the first `+` thread on the empty-project screen.
        panel.component.set_gateway_ready(bridge_available);
        if let Some(identity) = initial_identity {
            let saved_path = identity.saved_path().map(str::to_owned);
            let mut model = panel.model.borrow_mut();
            model.active_project = identity;
            model.active_project_path = saved_path;
            drop(model);
            if let Some(store) = panel.panel_state.clone() {
                panel
                    .project_state_stores
                    .borrow_mut()
                    .insert(panel.model.borrow().active_project.clone(), store);
            }
        }
        // Bind persistent VecModels before any callback can fire; content
        // arrives from the Init hydration + first Frame snapshot below.
        crate::sync::sync_initial_models(&panel.model.borrow(), &panel.component);
        {
            let mut model = panel.model.borrow_mut();
            let (effects, _) = update::update(&mut model, msg::Msg::Host(msg::HostMsg::Init));
            debug_assert!(matches!(
                effects.as_slice(),
                [effect::Effect::LoadInitialState]
            ));
            let (_, dirty) = update::update(
                &mut model,
                msg::Msg::Effect(effect::EffectResultMsg::InitialStateLoaded(Ok(initial))),
            );
            sync::sync(&model, &panel.component, &dirty);
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
        let mut post_hydration_warnings: Vec<String> = Vec::new();
        if let Some(store) = panel.panel_state.as_ref() {
            maybe_migrate_sqlite_defaults_to_json(store, &mut post_hydration_warnings);
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
        let scoped_prefs = load_scoped_panel_prefs(
            settings_scope,
            selected_from_sqlite.clone(),
            &mut post_hydration_warnings,
        );
        let defaults = scoped_prefs
            .as_ref()
            .map(|prefs| prefs.defaults.clone())
            .unwrap_or_else(|| {
                load_panel_prefs(selected_from_sqlite, &mut post_hydration_warnings)
            });
        panel.sync_runtime_defaults(&defaults);
        // bug4_new_thread_default_provider: `scoped_prefs.default_agent_id`
        // was computed above but never applied to `model.default_agent_id`
        // (what `ThreadMsg::New` reads for a new thread's provider) -- that
        // was previously only set via the Settings "Save" button. Apply it
        // here too so a fresh thread respects the configured default.
        if let Some(configured_agent_id) = scoped_prefs.as_ref().and_then(|prefs| {
            settings_file::non_default_sentinel(prefs.default_agent_id.clone())
        }) {
            panel.model.borrow_mut().default_agent_id = configured_agent_id;
        }
        for message in post_hydration_warnings {
            let _ = dispatch::update_persistent(
                &panel,
                msg::Msg::Effect(effect::EffectResultMsg::StateEffectFailed {
                    thread_id: String::new(),
                    message,
                }),
            );
        }
        if let Some(selected_thread_id) = defaults.selected_thread_id {
            if let Some(real_idx) = panel.bridge.as_ref().and_then(|bridge| {
                (0..panel.model.borrow().threads.len()).find(|idx| {
                    bridge
                        .thread_binding(*idx)
                        .is_some_and(|binding| binding.thread_id == selected_thread_id)
                })
            }) {
                let filtered_idx = {
                    panel
                        .model
                        .borrow()
                        .visible_indices
                        .iter()
                        .position(|idx| *idx == real_idx)
                };
                if let Some(filtered_idx) = filtered_idx {
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
                let message = format!("failed to install bundled global skill at startup: {error}");
                eprintln!("panel-rust: {message}");
                let _ = dispatch::update_persistent(
                    &panel,
                    msg::Msg::Effect(effect::EffectResultMsg::StateEffectFailed {
                        thread_id: String::new(),
                        message,
                    }),
                );
            }
            panel.dispatch_frame_input(crate::msg::FrameInput {
                skills_snapshot: Some(
                    crate::external_snapshot::ExternalSnapshotSource::new(&panel)
                        .collect_skills_snapshot(),
                ),
                ..crate::msg::FrameInput::default()
            });
        }
        let selected_thread = { panel.model.borrow().selected_thread };
        if let Some(real_idx) = panel.real_index(selected_thread) {
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
        panel.component.on_mcp_server_submit(move |data| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            let entry = models::mcp_server_entry_from_form(&data);
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    if data.is_edit {
                        dispatch::dispatch_mcp_server_update(panel, &component, entry);
                    } else {
                        dispatch::dispatch_mcp_server_create(panel, &component, entry);
                    }
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

        panel.component.on_md_link_activated(move |target| {
            open_md_link_target(target.as_str());
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_mcp_server_authenticate(move |name| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_mcp_server_authenticate(panel, &component, name.to_string());
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_mcp_server_logout(move |name| {
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_mcp_server_logout(panel, &component, name.to_string());
                }
            });
        });

        let component_weak = panel.component.as_weak();
        panel.component.on_mcp_server_tool_enabled_changed(
            move |server_name, tool_name, enabled| {
                let Some(component) = component_weak.upgrade() else {
                    return;
                };
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_mcp_server_tool_enabled_changed(
                            panel,
                            &component,
                            server_name.to_string(),
                            tool_name.to_string(),
                            enabled,
                        );
                    }
                });
            },
        );

        let component_weak = panel.component.as_weak();
        panel.component.on_mcp_server_tool_deferred_changed(
            move |server_name, tool_name, deferred| {
                let Some(component) = component_weak.upgrade() else {
                    return;
                };
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_mcp_server_tool_deferred_changed(
                            panel,
                            &component,
                            server_name.to_string(),
                            tool_name.to_string(),
                            deferred,
                        );
                    }
                });
            },
        );

        let component_weak = panel.component.as_weak();
        panel
            .component
            .on_mcp_server_tools_fetch_requested(move |server_name| {
                let Some(component) = component_weak.upgrade() else {
                    return;
                };
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_mcp_server_tools_fetch_requested(
                            panel,
                            &component,
                            server_name.to_string(),
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
        panel.component.on_agent_website_clicked(move |website| {
            let Some(_component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    panel.dispatch_agent_website_clicked(website.as_str());
                }
            });
        });

        panel
            .component
            .on_agent_set_enabled(move |agent_id, enabled| {
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_agent_set_enabled(panel, agent_id.to_string(), enabled);
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
        panel.component.on_send_requested(move |text| {
            let text = text.to_string().trim().to_owned();
            if text.is_empty() {
                trace_host_input("send requested with empty composer");
                return;
            }
            let Some(component) = component_weak.upgrade() else {
                return;
            };
            let filtered_idx = component.get_selected_thread() as usize;
            trace_host_input(format_args!(
                "send requested selected_thread={filtered_idx} text={text:?}"
            ));
            PANEL.with(move |cell| {
                // PUI-014: &mut so a deferred thread's first message can attach
                // its session (bound to the currently-selected provider) before
                // the send is dispatched.
                if let Some(panel) = cell.borrow_mut().as_mut() {
                    dispatch::dispatch_compose_send_maybe_attach(panel, filtered_idx, text);
                }
            });
        });

        panel.component.on_compose_draft_changed(move |text| {
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_compose_draft_changed(panel, text.to_string());
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

        panel
            .component
            .on_queue_cancel_requested(move |message_index| {
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_queue_cancel(panel, message_index as usize);
                    }
                });
            });
        panel
            .component
            .on_queue_edit_requested(move |message_index| {
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_queue_edit(panel, message_index as usize);
                    }
                });
            });
        panel.component.on_queue_stop_requested(move || {
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_queue_stop(panel);
                }
            });
        });
        panel
            .component
            .on_queue_send_now_requested(move |message_index| {
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_queue_send_now(panel, message_index as usize);
                    }
                });
            });
        panel.component.on_queue_fast_track_requested(move || {
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_queue_fast_track(panel);
                }
            });
        });

        // setup-followups plan, archive_thread_backend_verify: the
        // sidebar's Archive control was previously a UI-only stub with
        // no Rust-side wiring at all. Routed through the TEA dispatcher
        // (dispatch::dispatch_thread_archive), same as thread close/
        // delete above -- archive_thread itself never sends an ACP
        // request, it's a purely local, durable (see AgentBridge::
        // archive_thread's doc comment) presentation flag.
        panel
            .component
            .on_thread_archive_requested(move |filtered_idx| {
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_thread_archive(panel, filtered_idx as usize);
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

        // PUI-002b: terminals popup's `[x]` kill button.
        let component_weak = panel.component.as_weak();
        panel
            .component
            .on_terminal_kill_requested(move |terminal_id| {
                let Some(component) = component_weak.upgrade() else {
                    return;
                };
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_terminal_kill_requested(
                            panel,
                            &component,
                            terminal_id.to_string(),
                        );
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

        // Terminal-tabs phase: switch the active tab / dismiss one tab
        // from the overlay's open set, both fired from the tab strip
        // inside `TerminalOverlay` itself (not the popup).
        let component_weak = panel.component.as_weak();
        panel
            .component
            .on_terminal_tab_selected(move |terminal_id| {
                let Some(_component) = component_weak.upgrade() else {
                    return;
                };
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_terminal_tab_selected(panel, terminal_id.to_string());
                    }
                });
            });

        let component_weak = panel.component.as_weak();
        panel.component.on_terminal_tab_closed(move |terminal_id| {
            let Some(_component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_terminal_tab_closed(panel, terminal_id.to_string());
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

        panel
            .component
            .on_profile_selected(move |profile_name, agent_id| {
                PANEL.with(|cell| {
                    if let Some(panel) = cell.borrow().as_ref() {
                        dispatch::dispatch_profile_selected(
                            panel,
                            profile_name.to_string(),
                            agent_id.to_string(),
                        );
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

        let component_weak = panel.component.as_weak();
        panel.component.on_copy_message(move |text| {
            let Some(_component) = component_weak.upgrade() else {
                return;
            };
            PANEL.with(|cell| {
                if let Some(panel) = cell.borrow().as_ref() {
                    dispatch::dispatch_copy_message(panel, text.to_string());
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

    PANEL.with(|cell| {
        *cell.borrow_mut() = Some(panel);
    });
    &SENTINEL as *const PanelHandle as *mut PanelHandle
}

/// Cold-start variant used by the Qt adapter when it already has the host's
/// pending project lifecycle signal. Supplying the identity before creating
/// the bridge makes initial thread hydration read the project-local store.
#[no_mangle]
pub extern "C" fn panel_rust_create_with_identity(
    width: c_uint,
    height: c_uint,
    path_ptr: *const c_uchar,
    path_len: usize,
    untitled: bool,
) -> *mut PanelHandle {
    let identity = if untitled {
        Some(model::ProjectIdentity::Untitled(
            uuid::Uuid::new_v4().to_string(),
        ))
    } else if path_ptr.is_null() || path_len == 0 {
        None
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(path_ptr, path_len) };
        std::str::from_utf8(bytes)
            .ok()
            .map(|path| model::ProjectIdentity::Saved(path.to_owned()))
    };
    panel_rust_create_with_initial_identity(width, height, identity)
}

#[no_mangle]
pub extern "C" fn panel_rust_destroy(_handle: *mut PanelHandle) {
    // The C ABI handle is a process-local sentinel; the actual ownership is
    // the thread-local singleton. Clearing it drops AgentBridge and stops
    // local actors when Qt destroys or recreates the dock.
    let panel = PANEL.with(|cell| cell.borrow_mut().take());
    drop(panel);
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

#[no_mangle]
pub extern "C" fn panel_rust_project_created_untitled(_handle: *mut PanelHandle) -> bool {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        dispatch::dispatch_project_created_untitled(panel);
        true
    })
}

#[no_mangle]
pub extern "C" fn panel_rust_project_closed(_handle: *mut PanelHandle) -> bool {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        dispatch::dispatch_project_closed(panel);
        true
    })
}

/// PISO-7 (project-isolation-mlt-binding plan) FFI crossing point --
/// mirrors `panel_rust_set_project_path`'s byte-buffer shape, but takes
/// TWO strings (old path, then new path) since a rename is a pair, not a
/// single value. `ChatRustDock` should call this instead of
/// `panel_rust_set_project_path` specifically for an MLT Save-As (where
/// Shotcut knows both the path being replaced and its replacement);
/// every other project-path change (open, close, first save of an
/// untitled project) keeps going through `panel_rust_set_project_path`
/// as before. Passing a zero-length `old` buffer is equivalent to "not a
/// rename" and is a no-op on the Rust side (see `HostMsg::
/// ProjectPathRenamed`'s doc comment) -- callers with no old path should
/// call `panel_rust_set_project_path` instead of this function.
#[no_mangle]
pub extern "C" fn panel_rust_rename_project_path(
    _handle: *mut PanelHandle,
    old_path_ptr: *const c_uchar,
    old_path_len: usize,
    new_path_ptr: *const c_uchar,
    new_path_len: usize,
) -> bool {
    let old_path = if old_path_ptr.is_null() || old_path_len == 0 {
        String::new()
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(old_path_ptr, old_path_len) };
        std::str::from_utf8(bytes).unwrap_or_default().to_owned()
    };
    let new_path = if new_path_ptr.is_null() || new_path_len == 0 {
        String::new()
    } else {
        let bytes = unsafe { std::slice::from_raw_parts(new_path_ptr, new_path_len) };
        std::str::from_utf8(bytes).unwrap_or_default().to_owned()
    };
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        dispatch::dispatch_project_path_renamed(panel, old_path, new_path);
        true
    })
}

/// language-switch-sync plan: sets the active UI language from a QSettings
/// locale code (e.g. "fr", "zh_CN") -- mirrors `panel_rust_set_theme`'s/
/// `panel_rust_set_project_path`'s byte-buffer shape exactly.
/// `ChatRustDock` calls this once at construction (seeded from
/// `Settings.language()`, the cold-start value) and again live every time
/// `MainWindow::languageChanged` fires (a real switch in Qt's Settings >
/// Language menu) -- see that signal's own doc comment for why this is
/// wired as a genuine live signal rather than construction-time only
/// (the gap `panel_rust_set_theme`/`applyTheme` still has). An empty
/// buffer is a real no-op here (unlike theme's "empty means dark"
/// default) -- there's no sensible language to fall back to other than
/// "don't switch", so this returns `true` (the panel exists) without
/// dispatching anything.
#[no_mangle]
pub extern "C" fn panel_rust_set_language(
    _handle: *mut PanelHandle,
    text_ptr: *const c_uchar,
    text_len: usize,
) -> bool {
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        if text_ptr.is_null() || text_len == 0 {
            return true;
        }
        let bytes = unsafe { std::slice::from_raw_parts(text_ptr, text_len) };
        let Ok(text) = std::str::from_utf8(bytes) else {
            return true;
        };
        dispatch::dispatch_language_changed(panel, text.to_owned());
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
    snapshotd_lifecycle::heartbeat_if_due();
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
    // Drain closures queued by `SpikeEventLoopProxy::invoke_from_event_loop`
    // (e.g. `slint::spawn_local` futures' wakers) -- see `SpikePlatform::
    // new_event_loop_proxy`'s doc comment for why this is needed at all.
    let queued: Vec<_> = EVENT_LOOP_QUEUE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain(..)
        .collect();
    for event in queued {
        event();
    }
    PANEL.with(|cell| {
        let slot = cell.borrow();
        let Some(panel) = slot.as_ref() else {
            return false;
        };
        let frame_changed = dispatch::dispatch_frame_poll(panel);
        let animating = panel.window.window().has_active_animations();
        // audit-fixes animation_tick_loaders: progress bar, live-tail
        // pulse, and loaders are driven by `animation-tick()`, not
        // `animate` keyframes. `has_active_animations()` does not always
        // report that path as active, so without an explicit busy-thread
        // repaint the tick only advances on unrelated input (mouse move)
        // and the UI looks frozen. Any Loading/Cancelling thread (not
        // only the displayed one) needs continuous redraw so sidebar
        // spinners on other rows keep spinning too.
        //
        // Connecting... also drives the chat-header loader (chat_area
        // `header-spinning`) even before ThreadState flips to Loading.
        let busy_thread_animating = {
            let model = panel.model.borrow();
            model.threads.iter().any(|thread| {
                matches!(thread.state, ThreadState::Loading | ThreadState::Cancelling)
                    || thread.connection_status == "Connecting..."
            })
        };
        // mcp_servers_spinner_repaint_gap: the Settings > MCP Servers row
        // Spinners (Fetch tools / enable-toggle / Remove / Authenticate /
        // Logout, mcp_servers_view.slint) use this same animation-tick()
        // pattern but live entirely outside `model.threads` -- their busy
        // state is `available_mcp_servers`/`mcp_operations_in_flight`,
        // folded into per-row booleans by `models::to_mcp_server_option_
        // rows`. Without this check they got one correct frame at
        // click-time and then froze, same root cause as the
        // `busy_thread_animating` case above just never extended to this
        // model. Mirrors `to_mcp_server_option_rows`'s own `is_busy`
        // check rather than re-deriving the Slint-side McpServerOption
        // rows here.
        let busy_mcp_server_animating = {
            let model = panel.model.borrow();
            let is_busy = |action: &str, name: &str| {
                model
                    .mcp_operations_in_flight
                    .iter()
                    .any(|key| key == &format!("{action}:{name}"))
            };
            model.available_mcp_servers.iter().any(|entry| {
                is_busy("delete", &entry.name)
                    || is_busy("enabled", &entry.name)
                    || is_busy("authenticate", &entry.name)
                    || is_busy("logout", &entry.name)
                    || matches!(
                        entry.tool_catalog,
                        Some(crate::protocol_types::McpToolCatalog::Fetching)
                    )
                    || (entry.tool_catalog.is_none() && is_busy("tools_fetch", &entry.name))
            })
        };
        // With zero durable threads, App mounts `legacy-chat-area` instead of
        // a ChatViewStack delegate. That ChatArea still renders its
        // connection-status spinner and topbar shimmer from the component-
        // level default `connection-status == "Connecting..."`, but there is
        // no ThreadModel for the predicate above to inspect. Include the
        // component projection so the no-thread loading state keeps ticking
        // without requiring mouse movement.
        let component_connection_animating =
            panel.component.get_connection_status().as_str() == "Connecting...";
        let busy_animation = busy_thread_animating
            || busy_mcp_server_animating
            || component_connection_animating;
        let needs_paint = frame_changed || animating || busy_animation;
        // Critical: Qt's `requestRepaint` only re-blits the software
        // buffer. `MinimalSoftwareWindow::draw_if_needed` no-ops unless
        // Slint itself was told to redraw. `animation-tick()` bindings
        // do not always set that flag (unlike keyframed `animate`), so
        // a busy loader froze: poll returned true, paint ran, but the
        // buffer was never re-rendered. Force the flag whenever we
        // need continuous tick-driven paint.
        if needs_paint && (animating || busy_animation) {
            panel.window.window().request_redraw();
        }
        needs_paint
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

    fn mock_agent_bin_for_lifecycle_test() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/debug/rui-mock-agent")
    }

    fn acpx_server_bin_for_lifecycle_test() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../acpx/target/debug/acpx-server")
    }

    /// **`panel-rust-e2e-hardening`'s `messages_disappear_after_send`
    /// phase.** Real, headless (no VNC) reproduction of the reported
    /// "message disappears after send" bug, driving the exact same
    /// `dispatch_thread_new` -> `dispatch_compose_send` ->
    /// `panel_rust_poll` path the live app uses, against a real
    /// `acpx-server` + the free `rui-mock-agent` backend (no network
    /// auth needed, unlike the `ACPX_LIVE_TEST_AMBIENT`-gated codex
    /// tests in `agent_bridge.rs`).
    #[test]
    fn a_sent_message_stays_visible_across_several_poll_ticks() {
        use slint::Model as _;
        let cache_dir = tempfile::tempdir().expect("cache dir");
        let mock_agent = mock_agent_bin_for_lifecycle_test();
        assert!(
            mock_agent.is_file(),
            "target/debug/rui-mock-agent must exist -- run `cargo build --bin rui-mock-agent` first"
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);
        let mut command = std::process::Command::new(acpx_server_bin_for_lifecycle_test());
        command.env("ACPX_HTTP_BIND", format!("127.0.0.1:{port}"));
        // PROF-5: routed through the crate's one sanctioned in-crate-test
        // exemption (see agent_bridge.rs's own doc comment on
        // `test_only_set_backend_cmd_env`) instead of a raw `.env(...)`
        // write, so the backend_cmd_env_write_regression_test guard has
        // exactly one call site to recognize.
        crate::agent_bridge::test_only_set_backend_cmd_env(
            &mut command,
            mock_agent.to_string_lossy().into_owned(),
        )
        .env("ACPX_DEFAULT_AGENT_ID", "codex")
        .env("RUI_MOCK_AGENT_PERSONA", "codex")
        .env("RUST_LOG", "error")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
        let mut child = command.spawn().expect("spawn real acpx-server for test");
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(3000);
        let mut reachable = false;
        while std::time::Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                reachable = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        assert!(reachable, "test acpx-server never became reachable");

        let previous = [
            (
                "RUI_ACPX_CODEX_URL",
                std::env::var("RUI_ACPX_CODEX_URL").ok(),
            ),
            ("RUI_ACP_CACHE_DIR", std::env::var("RUI_ACP_CACHE_DIR").ok()),
        ];
        std::env::set_var("RUI_ACPX_CODEX_URL", format!("http://127.0.0.1:{port}"));
        std::env::set_var("RUI_ACP_CACHE_DIR", cache_dir.path());

        let project_path = cache_dir.path().join("lifecycle-test.mlt");
        let project_path = project_path.to_string_lossy().into_owned();
        let handle = panel_rust_create_with_identity(
            96,
            64,
            project_path.as_ptr(),
            project_path.len(),
            false,
        );
        assert!(!handle.is_null());

        // `dispatch_thread_new`'s `_component` parameter is unused in its
        // body (prefixed `_`) -- a throwaway instance sidesteps needing
        // to simultaneously borrow `panel` mutably and `panel.component`
        // (which isn't `Clone`) immutably.
        let throwaway_component = ChatPanel::new().expect("construct throwaway chat panel");
        PANEL.with(|cell| {
            let mut slot = cell.borrow_mut();
            let panel = slot.as_mut().expect("panel exists");
            dispatch::dispatch_thread_new(panel, &throwaway_component);
        });

        // PUI-014: the new thread is created DEFERRED -- it opens no session
        // until the first message is sent, so (unlike the old eager path) do
        // NOT wait for a binding before sending. Drive the send through the
        // same `&mut` attach-aware entry the live `on_send_requested` closure
        // uses: it attaches the deferred thread (bound to the currently
        // selected provider) and then sends. This exercises the real
        // first-send-attach path end to end against a real acpx-server.
        let selected_filtered_idx = PANEL.with(|cell| {
            let slot = cell.borrow();
            let panel = slot.as_ref().expect("panel exists");
            let idx = panel.model.borrow().selected_thread;
            idx
        });
        PANEL.with(|cell| {
            let mut slot = cell.borrow_mut();
            let panel = slot.as_mut().expect("panel exists");
            dispatch::dispatch_compose_send_maybe_attach(
                panel,
                selected_filtered_idx,
                "does this survive".to_owned(),
            );
        });

        // The deferred first-send attach is a real background session/new
        // round trip, so poll until the just-sent message first appears (with
        // a generous deadline covering the attach) before running the
        // "stays visible" regression check below.
        let appear_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            panel_rust_poll(handle);
            let present = PANEL.with(|cell| {
                let slot = cell.borrow();
                let panel = slot.as_ref().expect("panel exists");
                (0..panel.component.get_messages().row_count())
                    .filter_map(|i| panel.component.get_messages().row_data(i))
                    .any(|row| row.text.to_string().contains("does this survive"))
            });
            if present {
                break;
            }
            assert!(
                std::time::Instant::now() < appear_deadline,
                "the sent message never became visible after the deferred thread's first-send attach"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // The actual regression check: poll several times (simulating
        // several 60-90fps ticks) and confirm the just-sent message is
        // visible in the shared messages_model on every single tick, not
        // just some of them -- "disappears after send" would show up as
        // present on tick 1 and gone on a later tick.
        let mut seen_present = false;
        for _ in 0..20 {
            panel_rust_poll(handle);
            let rows: Vec<String> = PANEL.with(|cell| {
                let slot = cell.borrow();
                let panel = slot.as_ref().expect("panel exists");
                (0..panel.component.get_messages().row_count())
                    .filter_map(|i| panel.component.get_messages().row_data(i))
                    .map(|row| row.text.to_string())
                    .collect()
            });
            let present = rows.iter().any(|row| row.contains("does this survive"));
            if present {
                seen_present = true;
            } else if seen_present {
                panic!(
                    "the sent message was visible on an earlier poll tick but disappeared \
                     on a later one -- this is the reported live bug"
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        assert!(
            seen_present,
            "the sent message never became visible in messages_model across 20 poll ticks"
        );

        panel_rust_destroy(handle);
        let _ = child.kill();
        let _ = child.wait();
        for (key, value) in previous {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
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

            let project_path = cache_dir.path().join("test-panel.mlt");
            let project_path = project_path.to_string_lossy().into_owned();
            let handle = panel_rust_create_with_identity(
                240,
                260,
                project_path.as_ptr(),
                project_path.len(),
                false,
            );
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
            archived: false,
            provider: "".into(),
            model: "".into(),
            project_name: "".into(),
            project_path: "".into(),
            project_instance_live: false,
            profile_name: "".into(),
            has_session: false,
            relative_time: "now".into(),
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

    /// Select-and-copy regression test: before `SpikePlatform::
    /// set_clipboard_text` was implemented, a real Ctrl+C on a real
    /// selection was a silent no-op (the default `Platform` impl drops
    /// the text on the floor -- see that method's own doc comment).
    /// Drives the actual bug's repro end to end through the real FFI
    /// bridge: focus the compose box, select all its text with a real
    /// Ctrl+A chord (proving the selection itself is real, not poked in
    /// directly), then a real Ctrl+C chord -- both `panel_rust_input_key`
    /// calls take the same control-character-recovery path `map_qt_key`
    /// already normalizes Ctrl+B through in the test above. Asserts the
    /// exact selected text reached `Platform::set_clipboard_text`, via
    /// this environment's only honest observation point: there is no
    /// real X11/Wayland display here (confirmed: `xclip`/`wl-copy`/`xsel`
    /// all fail "can't open display"), so the real system clipboard
    /// itself cannot be asserted against headlessly -- see `set_clipboard_
    /// text`'s own doc comment for why `LAST_CLIPBOARD_WRITE_FOR_TEST`
    /// exists as the substitute.
    #[test]
    fn ctrl_c_on_a_real_selection_reaches_the_platform_clipboard_hook() {
        const QT_KEY_A: c_int = 0x41;
        const QT_KEY_C: c_int = 0x43;

        let panel = TestPanel::new();
        let component = panel.component();
        component.set_compose_text("select and copy me".into());

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
        *LAST_CLIPBOARD_WRITE_FOR_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;

        panel.press_only(QT_KEY_CONTROL);
        panel.press_and_release(QT_KEY_A, "\u{1}");
        panel.press_and_release(QT_KEY_C, "\u{3}");
        panel.release_only(QT_KEY_CONTROL);

        assert_eq!(
            LAST_CLIPBOARD_WRITE_FOR_TEST
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
            Some("select and copy me".to_owned()),
            "a real Ctrl+A-then-Ctrl+C chord on the compose box's full selection must reach \
             SpikePlatform::set_clipboard_text with exactly the selected text"
        );
    }
}

/// Phase 17 (`markdown_highlight_and_real_links`): opens a Ctrl+Clicked
/// markdown link target -- file paths and external URLs both go through
/// the platform opener (`xdg-open` on Linux). `RUI_LINK_OPEN_CMD`
/// overrides the command for e2e tests, which point it at a recorder
/// script and assert the exact target that was passed through.
pub(crate) fn open_md_link_target(target: &str) {
    let target = target.trim();
    if target.is_empty() {
        return;
    }
    let opener = std::env::var("RUI_LINK_OPEN_CMD").unwrap_or_else(|_| "xdg-open".to_string());
    match std::process::Command::new(&opener)
        .arg(target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        // Reap on a detached thread -- dropping the Child without wait()
        // left one defunct opener process per Ctrl+Click for the panel's
        // lifetime (review-gate finding; effect_executor's child-process
        // site already reaps the same way).
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(error) => {
            eprintln!("panel-rust: failed to open link {target:?} via {opener:?}: {error}")
        }
    }
}
