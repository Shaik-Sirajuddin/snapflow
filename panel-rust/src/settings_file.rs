//! Multi-process panel settings: JSON files + optional file poll watch.
//!
//! Global/Project prefs (profile, permission label, background default,
//! harness flags) live here so independent OS processes can share Global
//! via a watched path. This is **not** acpx-server boot config — the panel
//! reads these defaults and calls acpx over HTTP/WS.
//!
//! See `memory/rui/gen/plans/settings-panel/settings-live-acpx-wiring.md`.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HarnessSettings {
    #[serde(default = "default_true")]
    pub notifications_enabled: bool,
    #[serde(default = "default_true")]
    pub notify_on_input_required: bool,
    #[serde(default)]
    pub auto_resume_on_rate_limit_reset: bool,
}

fn default_true() -> bool {
    true
}

impl Default for HarnessSettings {
    fn default() -> Self {
        Self {
            notifications_enabled: true,
            notify_on_input_required: true,
            auto_resume_on_rate_limit_reset: false,
        }
    }
}

/// Sparse document: missing fields mean "inherit from lower layer".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsDocument {
    #[serde(default = "schema_version_default")]
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_session_default: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<HarnessSettings>,
    // `dev_mode` -- `dev-mode` task's "Dev mode option for the system".
    // Deliberately Global tier only: read/written directly against
    // `SettingsPaths::global` by `SettingsPaths::dev_mode()`/
    // `set_dev_mode()` below, bypassing `merge_documents`'s generic
    // Project-overrides-Global layering entirely -- a project-scoped
    // settings.json setting this key has no effect, since dev mode is a
    // system-level toggle, not a per-project one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dev_mode: Option<bool>,
}

fn schema_version_default() -> u32 {
    1
}

/// Fully resolved prefs after Project → Global → bundled default merge.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedSettings {
    pub default_profile: Option<String>,
    pub permission_profile: Option<String>,
    pub background_session_default: bool,
    pub default_agent_id: Option<String>,
    pub harness: HarnessSettings,
}

impl Default for ResolvedSettings {
    fn default() -> Self {
        Self {
            default_profile: None,
            permission_profile: None,
            background_session_default: false,
            default_agent_id: None,
            harness: HarnessSettings::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsFileError {
    #[error("IO error for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("JSON error for {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// Merge layers: later wins only when it sets a field (`Some`).
pub fn merge_documents(layers: &[&SettingsDocument]) -> ResolvedSettings {
    let mut out = ResolvedSettings::default();
    for doc in layers {
        if let Some(ref v) = doc.default_profile {
            out.default_profile = non_empty_opt(v.clone());
        }
        if let Some(ref v) = doc.permission_profile {
            out.permission_profile = non_empty_opt(v.clone());
        }
        if let Some(v) = doc.background_session_default {
            out.background_session_default = v;
        }
        if let Some(ref v) = doc.default_agent_id {
            out.default_agent_id = non_empty_opt(v.clone());
        }
        if let Some(ref h) = doc.harness {
            out.harness = h.clone();
        }
    }
    out
}

fn non_empty_opt(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

pub fn load_document(path: &Path) -> Result<SettingsDocument, SettingsFileError> {
    if !path.exists() {
        return Ok(SettingsDocument::default());
    }
    let bytes = fs::read(path).map_err(|source| SettingsFileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(SettingsDocument::default());
    }
    serde_json::from_slice(&bytes).map_err(|source| SettingsFileError::Json {
        path: path.to_path_buf(),
        source,
    })
}

/// Atomic write: `path.tmp` → fsync → rename over `path`.
pub fn save_document(path: &Path, doc: &SettingsDocument) -> Result<(), SettingsFileError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| SettingsFileError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(doc).map_err(|source| SettingsFileError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    {
        let mut f = fs::File::create(&tmp).map_err(|source| SettingsFileError::Io {
            path: tmp.clone(),
            source,
        })?;
        f.write_all(&json).map_err(|source| SettingsFileError::Io {
            path: tmp.clone(),
            source,
        })?;
        f.write_all(b"\n").map_err(|source| SettingsFileError::Io {
            path: tmp.clone(),
            source,
        })?;
        // audit-fixes offload_persist_sync_all: skip fsync on settings
        // write (UI-path latency); atomic rename is enough for small JSON.
        drop(f);
    }
    fs::rename(&tmp, path).map_err(|source| SettingsFileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Paths for global / project / bundled defaults.
#[derive(Clone, Debug)]
pub struct SettingsPaths {
    pub global: PathBuf,
    pub project: Option<PathBuf>,
    pub bundled_default: Option<PathBuf>,
}

impl SettingsPaths {
    /// Resolve from env:
    /// - `RUI_PANEL_SETTINGS_DIR` → `{dir}/settings.global.json`
    /// - else `{RUI_ACP_CACHE_DIR}/../panel-settings/settings.global.json`
    /// - else `{HOME}/.config/panel-rust/settings.global.json`
    /// - project: `RUI_PANEL_PROJECT_ROOT/.snapshot/panel-settings.json`
    pub fn from_env() -> Self {
        let global_dir = std::env::var_os("RUI_PANEL_SETTINGS_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("RUI_ACP_CACHE_DIR").map(|c| {
                    PathBuf::from(c)
                        .parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join("panel-settings")
                })
            })
            .unwrap_or_else(|| dirs_fallback_config().join("panel-rust"));
        let global = global_dir.join("settings.global.json");
        let project = std::env::var_os("RUI_PANEL_PROJECT_ROOT").map(|r| {
            PathBuf::from(r)
                .join(".snapshot")
                .join("panel-settings.json")
        });
        let bundled_default = std::env::var_os("RUI_PANEL_SETTINGS_DEFAULT")
            .map(PathBuf::from)
            .or_else(|| {
                // Dev checkout: panel-rust/settings.default.json next to CARGO_MANIFEST_DIR
                let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("settings.default.json");
                p.exists().then_some(p)
            });
        Self {
            global,
            project,
            bundled_default,
        }
    }

    pub fn load_resolved(&self) -> Result<ResolvedSettings, SettingsFileError> {
        let mut layers: Vec<SettingsDocument> = Vec::new();
        if let Some(ref b) = self.bundled_default {
            layers.push(load_document(b)?);
        }
        layers.push(load_document(&self.global)?);
        if let Some(ref p) = self.project {
            layers.push(load_document(p)?);
        }
        let refs: Vec<&SettingsDocument> = layers.iter().collect();
        Ok(merge_documents(&refs))
    }

    /// Reads `dev_mode` from the Global document only -- see
    /// `SettingsDocument::dev_mode`'s doc comment for why this
    /// deliberately does not go through `load_resolved`'s generic
    /// Project-overrides-Global merge.
    pub fn dev_mode(&self) -> bool {
        load_document(&self.global)
            .ok()
            .and_then(|doc| doc.dev_mode)
            .unwrap_or(false)
    }

    /// Writes `dev_mode` into the Global document, preserving every
    /// other field already on disk (same read-modify-write shape
    /// `lib.rs::save_panel_prefs_to_json` already uses for the other
    /// Global-tier fields).
    pub fn set_dev_mode(&self, enabled: bool) -> Result<(), SettingsFileError> {
        let mut doc = load_document(&self.global)?;
        doc.schema_version = 1;
        doc.dev_mode = Some(enabled);
        save_document(&self.global, &doc)
    }
}

fn dirs_fallback_config() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config");
    }
    PathBuf::from(".config")
}

/// The literal string `"default"` is never a real, assignable ACPX
/// profile name in this system -- auto-seeded profiles are named after
/// their real agent id (e.g. `"codex-acp"`), never `"default"` itself
/// (see acpx-core's `Router::ensure_default_profiles_seeded`). A stale or
/// hand-edited settings file that persisted this literal string as a
/// "chosen" profile must not be forwarded to `session/new` as a real
/// profile name -- doing so fails every new session with "no profile
/// named default" instead of falling back to native/unmanaged mode the
/// way an actually-empty value would. Found live: a real settings.global.
/// json on a real dev machine had exactly this value, silently breaking
/// every new thread's session/new call with no indication why.
pub(crate) fn non_default_sentinel(value: Option<String>) -> Option<String> {
    value.filter(|v| v != "default")
}

/// Map resolved settings into the legacy [`crate::state_store::PanelDefaults`]
/// fields (without selected_thread_id — that stays process-local).
pub fn resolved_to_panel_defaults(
    resolved: &ResolvedSettings,
    selected_thread_id: Option<String>,
) -> crate::state_store::PanelDefaults {
    crate::state_store::PanelDefaults {
        profile_name: non_default_sentinel(resolved.default_profile.clone()),
        permission_profile: non_default_sentinel(resolved.permission_profile.clone()),
        background_session: resolved.background_session_default,
        selected_thread_id,
    }
}

/// Poll mtime of global (+ project if set); on change call `on_change`.
/// Debounced. Stops when `stop` is set.
pub struct SettingsWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl SettingsWatcher {
    pub fn start(
        paths: SettingsPaths,
        debounce: Duration,
        on_change: Arc<Mutex<dyn FnMut(ResolvedSettings) + Send>>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let handle = thread::spawn(move || {
            let mut last_global = file_mtime(&paths.global);
            let mut last_project = paths
                .project
                .as_ref()
                .map(|p| file_mtime(p))
                .unwrap_or(None);
            while !stop_t.load(Ordering::SeqCst) {
                thread::sleep(debounce);
                if stop_t.load(Ordering::SeqCst) {
                    break;
                }
                let g = file_mtime(&paths.global);
                let p = paths
                    .project
                    .as_ref()
                    .map(|p| file_mtime(p))
                    .unwrap_or(None);
                if g != last_global || p != last_project {
                    last_global = g;
                    last_project = p;
                    if let Ok(resolved) = paths.load_resolved() {
                        if let Ok(mut cb) = on_change.lock() {
                            cb(resolved);
                        }
                    }
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for SettingsWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn merge_project_overrides_global() {
        let bundled = SettingsDocument {
            schema_version: 1,
            default_profile: Some("bundled".into()),
            permission_profile: Some("read".into()),
            background_session_default: Some(false),
            default_agent_id: None,
            harness: None,
            dev_mode: None,
        };
        let global = SettingsDocument {
            schema_version: 1,
            default_profile: Some("global-prof".into()),
            permission_profile: None,
            background_session_default: Some(true),
            default_agent_id: Some("codex".into()),
            harness: None,
            dev_mode: None,
        };
        let project = SettingsDocument {
            schema_version: 1,
            default_profile: Some("project-prof".into()),
            permission_profile: None,
            background_session_default: None,
            default_agent_id: None,
            harness: Some(HarnessSettings {
                notifications_enabled: false,
                notify_on_input_required: true,
                auto_resume_on_rate_limit_reset: true,
            }),
            dev_mode: None,
        };
        let r = merge_documents(&[&bundled, &global, &project]);
        assert_eq!(r.default_profile.as_deref(), Some("project-prof"));
        assert_eq!(r.permission_profile.as_deref(), Some("read"));
        assert!(r.background_session_default);
        assert_eq!(r.default_agent_id.as_deref(), Some("codex"));
        assert!(!r.harness.notifications_enabled);
        assert!(r.harness.auto_resume_on_rate_limit_reset);
    }

    #[test]
    fn resolved_to_panel_defaults_strips_the_literal_default_sentinel() {
        // Regression test: "agent default is in crash backoff". merge_documents
        // itself deliberately does NOT filter "default" (save_load_round_trip
        // below documents that raw round-tripping) -- resolved_to_panel_defaults
        // is the one place that must, since its output flows straight into
        // _acpx.profile on session/new, and acpx-server has no real backend
        // ever registered under the literal agent id "default" (see
        // acpxmgr.go's WriteConfig doc comment).
        let poisoned = ResolvedSettings {
            default_profile: Some("default".to_owned()),
            permission_profile: Some("default".to_owned()),
            background_session_default: true,
            default_agent_id: Some("codex".to_owned()),
            harness: HarnessSettings::default(),
        };
        let defaults = resolved_to_panel_defaults(&poisoned, None);
        assert_eq!(defaults.profile_name, None);
        assert_eq!(defaults.permission_profile, None);

        // A real, non-sentinel profile name must still pass through
        // unchanged -- this isn't a blanket "always clear" fallback.
        let real = ResolvedSettings {
            default_profile: Some("my-real-profile".to_owned()),
            permission_profile: Some("workspace".to_owned()),
            ..poisoned.clone()
        };
        let defaults = resolved_to_panel_defaults(&real, None);
        assert_eq!(defaults.profile_name.as_deref(), Some("my-real-profile"));
        assert_eq!(defaults.permission_profile.as_deref(), Some("workspace"));
    }

    #[test]
    fn a_fresh_install_with_no_settings_files_never_resolves_to_the_default_sentinel() {
        // Regression test for the "does this trace back to the install
        // path" question: a brand new user who has never touched Settings
        // has no Global/Project settings.json at all. merge_documents with
        // zero layers (mirroring `load_all`'s behavior when neither file
        // exists) must resolve to ResolvedSettings::default() --
        // default_profile: None -- not the literal "default" string. The
        // poisoned value seen in the wild came from a settings *save*
        // round-trip (now fixed at both the settings_file and update.rs
        // layers), not from how a fresh install starts out.
        let resolved = merge_documents(&[]);
        assert_eq!(resolved.default_profile, None);
        assert_eq!(resolved.permission_profile, None);
        assert_eq!(resolved.default_agent_id, None);

        let defaults = resolved_to_panel_defaults(&resolved, None);
        assert_eq!(defaults.profile_name, None);
        assert_eq!(defaults.permission_profile, None);
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("settings.global.json");
        let doc = SettingsDocument {
            schema_version: 1,
            default_profile: Some("default".into()),
            permission_profile: Some("full".into()),
            background_session_default: Some(true),
            default_agent_id: None,
            harness: Some(HarnessSettings::default()),
            dev_mode: None,
        };
        save_document(&path, &doc).unwrap();
        let loaded = load_document(&path).unwrap();
        assert_eq!(loaded.default_profile.as_deref(), Some("default"));
        assert_eq!(loaded.permission_profile.as_deref(), Some("full"));
        assert_eq!(loaded.background_session_default, Some(true));
    }

    #[test]
    fn dev_mode_defaults_to_false_and_round_trips_through_global_only() {
        let dir = tempdir().unwrap();
        let paths = SettingsPaths {
            global: dir.path().join("settings.global.json"),
            project: Some(dir.path().join("settings.project.json")),
            bundled_default: None,
        };
        assert!(!paths.dev_mode());

        paths.set_dev_mode(true).unwrap();
        assert!(paths.dev_mode());

        // A project-tier document setting dev_mode has no effect --
        // dev_mode() only ever reads the Global document.
        save_document(
            paths.project.as_ref().unwrap(),
            &SettingsDocument {
                dev_mode: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(paths.dev_mode());

        paths.set_dev_mode(false).unwrap();
        assert!(!paths.dev_mode());
    }

    #[test]
    fn set_dev_mode_preserves_other_global_fields() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("settings.global.json");
        save_document(
            &global,
            &SettingsDocument {
                default_profile: Some("kept".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let paths = SettingsPaths {
            global: global.clone(),
            project: None,
            bundled_default: None,
        };
        paths.set_dev_mode(true).unwrap();
        let reloaded = load_document(&global).unwrap();
        assert_eq!(reloaded.default_profile.as_deref(), Some("kept"));
        assert_eq!(reloaded.dev_mode, Some(true));
    }

    #[test]
    fn missing_file_is_empty_doc() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.json");
        let d = load_document(&path).unwrap();
        assert_eq!(d, SettingsDocument::default());
    }

    #[test]
    fn watcher_fires_on_external_write() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("settings.global.json");
        save_document(
            &global,
            &SettingsDocument {
                schema_version: 1,
                default_profile: Some("a".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let paths = SettingsPaths {
            global: global.clone(),
            project: None,
            bundled_default: None,
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_c = seen.clone();
        let watcher = SettingsWatcher::start(
            paths,
            Duration::from_millis(50),
            Arc::new(Mutex::new(move |r: ResolvedSettings| {
                seen_c.lock().unwrap().push(r.default_profile.clone());
            })),
        );
        thread::sleep(Duration::from_millis(80));
        save_document(
            &global,
            &SettingsDocument {
                schema_version: 1,
                default_profile: Some("b".into()),
                ..Default::default()
            },
        )
        .unwrap();
        // wait for debounce + poll
        for _ in 0..40 {
            if seen
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.as_deref() == Some("b"))
            {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        watcher.stop();
        let profiles = seen.lock().unwrap().clone();
        assert!(
            profiles.iter().any(|p| p.as_deref() == Some("b")),
            "expected watcher to observe profile b, got {profiles:?}"
        );
    }

    /// `settings_reflection_matrix` phase (skills-settings-e2e-verification
    /// plan): a small curated matrix of setting-change -> file-reflection ->
    /// live-pickup scenarios, per tasks/v2/init.yaml's testing-a block
    /// ("matrix to test different scenarios, different options").
    ///
    /// Deliberately scoped to panel-rust's own `SettingsWatcher` -- the
    /// real, already-shipped live-pickup mechanism `lib.rs`'s
    /// `settings_reload_pending`/`apply_json_prefs_to_component` uses to
    /// notice an externally-rewritten settings file while the panel is
    /// running. Direct investigation confirmed acpx-server has no config
    /// hot-reload mechanism at all (`ACPX_ACP_BRIDGE_CONFIG_FILE` is read
    /// once at startup only) -- there is nothing real to test at that layer
    /// without first building a feature that doesn't exist, which is out of
    /// scope for a test-only phase. This matrix covers every field
    /// `ResolvedSettings` exposes, plus Project-overrides-Global precedence
    /// and dev_mode's deliberate Global-tier-only exception, all through the
    /// same watcher path a live conversation actually observes settings
    /// changes through.
    #[test]
    fn settings_reflection_matrix() {
        let dir = tempdir().unwrap();
        let global = dir.path().join("settings.global.json");
        let project = dir.path().join("settings.project.json");
        save_document(&global, &SettingsDocument::default()).unwrap();
        let paths = SettingsPaths {
            global: global.clone(),
            project: Some(project.clone()),
            bundled_default: None,
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_c = seen.clone();
        let watcher = SettingsWatcher::start(
            paths,
            Duration::from_millis(50),
            Arc::new(Mutex::new(move |r: ResolvedSettings| {
                seen_c.lock().unwrap().push(r);
            })),
        );
        thread::sleep(Duration::from_millis(80));

        // Scenario 1: default_profile set at Global tier.
        save_document(
            &global,
            &SettingsDocument {
                schema_version: 1,
                default_profile: Some("gpt-profile".into()),
                ..Default::default()
            },
        )
        .unwrap();
        wait_for(&seen, |r| {
            r.default_profile.as_deref() == Some("gpt-profile")
        });

        // Scenario 2: permission_profile and background_session_default set
        // together at Global tier, in the same write.
        save_document(
            &global,
            &SettingsDocument {
                schema_version: 1,
                default_profile: Some("gpt-profile".into()),
                permission_profile: Some("readonly".into()),
                background_session_default: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        wait_for(&seen, |r| {
            r.permission_profile.as_deref() == Some("readonly") && r.background_session_default
        });

        // Scenario 3: default_agent_id set at Global tier.
        save_document(
            &global,
            &SettingsDocument {
                schema_version: 1,
                default_profile: Some("gpt-profile".into()),
                permission_profile: Some("readonly".into()),
                background_session_default: Some(true),
                default_agent_id: Some("claude".into()),
                ..Default::default()
            },
        )
        .unwrap();
        wait_for(&seen, |r| r.default_agent_id.as_deref() == Some("claude"));

        // Scenario 4: Project-tier override takes precedence over the
        // Global value already on disk, without touching Global at all.
        save_document(
            &project,
            &SettingsDocument {
                schema_version: 1,
                default_profile: Some("project-only-profile".into()),
                ..Default::default()
            },
        )
        .unwrap();
        wait_for(&seen, |r| {
            r.default_profile.as_deref() == Some("project-only-profile")
        });

        // Scenario 5: removing the Project-tier override (writing an empty
        // document back) falls back to the Global value again -- proves the
        // precedence isn't a one-way ratchet.
        save_document(&project, &SettingsDocument::default()).unwrap();
        wait_for(&seen, |r| {
            r.default_profile.as_deref() == Some("gpt-profile")
        });

        watcher.stop();

        // Scenario 6 (dev_mode's Global-tier-only exception): a
        // Project-tier dev_mode write must never reach ResolvedSettings at
        // all -- dev_mode is deliberately not part of the generic
        // Project-overrides-Global merge (see SettingsDocument::dev_mode's
        // doc comment), so it's read directly via SettingsPaths::dev_mode()
        // rather than through the watcher's ResolvedSettings callback.
        let paths_after = SettingsPaths {
            global: global.clone(),
            project: Some(project.clone()),
            bundled_default: None,
        };
        assert!(
            !paths_after.dev_mode(),
            "dev_mode should still be false before this scenario"
        );
        save_document(
            &project,
            &SettingsDocument {
                schema_version: 1,
                dev_mode: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            !paths_after.dev_mode(),
            "a Project-tier dev_mode write must have zero effect -- dev_mode is Global-tier only"
        );
        paths_after.set_dev_mode(true).unwrap();
        assert!(
            paths_after.dev_mode(),
            "dev_mode written via set_dev_mode (Global tier) must take effect"
        );
    }

    /// Polls `seen`'s most recent entries for up to ~2s, matching
    /// `watcher_fires_on_external_write`'s own poll shape -- shared here
    /// since `settings_reflection_matrix` needs it at 5 distinct points.
    fn wait_for(
        seen: &Arc<Mutex<Vec<ResolvedSettings>>>,
        matches: impl Fn(&ResolvedSettings) -> bool,
    ) {
        for _ in 0..40 {
            if seen.lock().unwrap().iter().any(&matches) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let snapshot = seen.lock().unwrap().clone();
        panic!(
            "expected watcher to observe a matching ResolvedSettings within ~2s, got {snapshot:?}"
        );
    }
}
