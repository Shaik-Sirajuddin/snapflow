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
        f.sync_all().map_err(|source| SettingsFileError::Io {
            path: tmp.clone(),
            source,
        })?;
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
            .unwrap_or_else(|| {
                dirs_fallback_config().join("panel-rust")
            });
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

/// Map resolved settings into the legacy [`crate::state_store::PanelDefaults`]
/// fields (without selected_thread_id — that stays process-local).
pub fn resolved_to_panel_defaults(
    resolved: &ResolvedSettings,
    selected_thread_id: Option<String>,
) -> crate::state_store::PanelDefaults {
    crate::state_store::PanelDefaults {
        profile_name: resolved.default_profile.clone(),
        permission_profile: resolved.permission_profile.clone(),
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
    use std::fs;
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
        };
        let global = SettingsDocument {
            schema_version: 1,
            default_profile: Some("global-prof".into()),
            permission_profile: None,
            background_session_default: Some(true),
            default_agent_id: Some("codex".into()),
            harness: None,
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
        };
        save_document(&path, &doc).unwrap();
        let loaded = load_document(&path).unwrap();
        assert_eq!(loaded.default_profile.as_deref(), Some("default"));
        assert_eq!(loaded.permission_profile.as_deref(), Some("full"));
        assert_eq!(loaded.background_session_default, Some(true));
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
}
