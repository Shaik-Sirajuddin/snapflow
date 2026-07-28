//! Agent auto-detection: per registry entry, checks whether its
//! distribution method's runtime is available. Phase 2 step 6.
//!
//! **`agents/list` latency fix.** `which()` used to spawn a real
//! subprocess (`Command::new(bin).arg("--version")...status()`) for every
//! single check. `detect()` runs once per registry entry inside
//! `agents/list` -- 38 entries in the real bundled registry as of this
//! fix, most `npx`-distributed (2 subprocess spawns each: `node`, `npm`)
//! -- so one `agents/list` call spawned dozens of child processes
//! synchronously on the async executor thread. Measured live: ~1.1s for
//! a call that does no I/O beyond "is this binary on PATH", reproduced
//! identically through both the strict `/acp` bridge and native `/rpc`
//! (ruling out the bridge-side `refresh_models` cooldown fix elsewhere in
//! this round as the cause -- this is a separate, independent latency
//! bug in `agents/list` itself). Replaced with a pure `PATH` directory
//! scan (`std::fs::metadata`, no exec) plus a short-TTL cache -- the
//! runtime environment (what's on `PATH`) essentially never changes
//! within one `acpx-server` process lifetime, so this is not a
//! correctness/staleness tradeoff in practice, just a "you added a
//! runtime and need to restart acpx to see it" edge case that already
//! existed for far more central config anyway.

use acpx_proto::agent::AgentStatus;
use acpx_registry::Distribution;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a `which()` result is trusted before being re-checked. Kept
/// short (not "forever") so a runtime installed *after* acpx-server
/// started (e.g. `uv` installed mid-session) is picked up without a
/// restart, just not on literally the next request.
const WHICH_CACHE_TTL: Duration = Duration::from_secs(300);

/// Make Node-based ACP runtimes visible when acpx-server itself was started
/// by systemd (or another non-login supervisor). Such services intentionally
/// receive a small, deterministic PATH and therefore do not source nvm's
/// shell initialization. Follow Omni's production fallback: preserve a
/// working global runtime, otherwise select the newest complete nvm prefix
/// under `$NVM_DIR` or `~/.nvm` and prepend its bin directory.
///
/// This is called once during acpx-server startup, before config resolution,
/// agent detection, or any backend process is spawned. Children inherit the
/// repaired PATH, so discovery and launch use the same Node/npm/npx prefix.
pub fn prepare_node_runtime_path() -> Option<PathBuf> {
    if node_runtime_available_on_path() {
        return None;
    }

    if let Some(home) = std::env::var_os("SNAPFLOW_ACP_NODE_HOME") {
        let bin_dir = PathBuf::from(home).join("bin");
        if complete_node_prefix(&bin_dir) {
            prepend_path(&bin_dir);
            return Some(bin_dir);
        }
    }

    let nvm_dir = std::env::var_os("NVM_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".nvm")))?;
    let bin_dir = newest_nvm_node_bin(&nvm_dir)?;
    prepend_path(&bin_dir);
    Some(bin_dir)
}

fn node_runtime_available_on_path() -> bool {
    ["node", "npm", "npx"]
        .iter()
        .all(|name| which_uncached(name))
}

fn complete_node_prefix(bin_dir: &Path) -> bool {
    ["node", "npm", "npx"]
        .iter()
        .all(|name| is_executable_file(&bin_dir.join(name)))
}

fn newest_nvm_node_bin(nvm_dir: &Path) -> Option<PathBuf> {
    let pattern = nvm_dir.join("versions").join("node");
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    let entries = std::fs::read_dir(pattern).ok()?;
    for entry in entries.flatten() {
        let bin_dir = entry.path().join("bin");
        if !complete_node_prefix(&bin_dir) {
            continue;
        }
        let Ok(node_mtime) =
            std::fs::metadata(bin_dir.join("node")).and_then(|meta| meta.modified())
        else {
            continue;
        };
        if newest
            .as_ref()
            .map_or(true, |(mtime, _)| node_mtime > *mtime)
        {
            newest = Some((node_mtime, bin_dir));
        }
    }
    newest.map(|(_, path)| path)
}

fn prepend_path(bin_dir: &Path) {
    let mut paths = vec![bin_dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    if let Ok(path) = std::env::join_paths(paths) {
        std::env::set_var("PATH", path);
    }
}

/// Best-effort detection for a single registry entry's preferred
/// distribution method.
///
/// - `npx`/`uvx`: runtime on `PATH` **and** a ready marker under
///   `~/.acpx/adapters/<id>/.ready` (written by `agents/install` after a
///   real package pre-fetch -- see `agents-install-runtime` plan). Runtime
///   present but no marker → `NotInstalled` (not "Installed" solely because
///   Node is available).
/// - `binary`: checks `~/.acpx/adapters/<id>/` for an already-fetched copy.
pub fn detect(agent_id: &str, dist: &Distribution) -> AgentStatus {
    match dist.preferred_method() {
        Some("npx") => {
            if !(which_node_tool("node") && which_node_tool("npm")) {
                AgentStatus::RuntimeMissing
            } else if package_ready(agent_id) {
                AgentStatus::Installed
            } else {
                AgentStatus::NotInstalled
            }
        }
        Some("uvx") => {
            if !which("uv") {
                AgentStatus::RuntimeMissing
            } else if package_ready(agent_id) {
                AgentStatus::Installed
            } else {
                AgentStatus::NotInstalled
            }
        }
        Some("binary") => {
            let adapter_dir = adapters_dir().join(agent_id);
            if adapter_dir.exists() {
                AgentStatus::Installed
            } else {
                AgentStatus::NotInstalled
            }
        }
        _ => AgentStatus::NotInstalled,
    }
}

fn adapters_dir() -> std::path::PathBuf {
    acpx_registry::default_adapters_dir()
}

fn package_ready(agent_id: &str) -> bool {
    acpx_registry::is_package_ready(&adapters_dir(), agent_id)
}

/// Prefer bundled bin when SNAPFLOW_ACP_NODE_HOME is set (acpxmgr injects
/// this only when bundled wins — global-first is decided at spawn time).
/// Otherwise PATH via which(). Matches which()'s executable check so a
/// non-executable stub is not treated as Installed.
fn which_node_tool(bin: &str) -> bool {
    if let Ok(home) = std::env::var("SNAPFLOW_ACP_NODE_HOME") {
        let p = std::path::Path::new(&home).join("bin").join(bin);
        if is_executable_file(&p) {
            return true;
        }
        // Home set but incomplete → do not fall through to a different
        // global bin (would mix prefixes). Treat as missing this tool.
        return false;
    }
    which(bin)
}

/// Drop cached `which()` results so a runtime installed after acpx started
/// (or an install that just completed) is visible on the next `agents/list`
/// without waiting for [`WHICH_CACHE_TTL`] or restarting the process.
pub fn invalidate_which_cache() {
    which_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

fn which(bin: &str) -> bool {
    {
        // Self-heals on poison (see `bridge_sessions::lock_sessions`'s
        // doc comment for the identical reasoning) rather than
        // permanently breaking every future `agents/status` binary
        // detection for this process's remaining lifetime.
        let cache = which_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((found, since)) = cache.get(bin) {
            if since.elapsed() < WHICH_CACHE_TTL {
                return *found;
            }
        }
    }
    let found = which_uncached(bin);
    which_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(bin.to_string(), (found, Instant::now()));
    found
}

fn which_cache() -> &'static Mutex<HashMap<String, (bool, Instant)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (bool, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Exposed to this module's own tests only -- lets a test observe that a
/// `which()` call actually populated the cache, without needing to mutate
/// process-wide `PATH` (which would race every other test in this crate's
/// shared lib-test binary that spawns a real `sh` subprocess -- see
/// `acpx-core/tests/gateway_native_coverage_test.rs`'s module doc comment
/// for the exact same hazard already documented there).
#[cfg(test)]
fn cache_entry_for_test(bin: &str) -> Option<(bool, Instant)> {
    which_cache()
        .lock()
        .expect("which cache poisoned")
        .get(bin)
        .copied()
}

/// Pure `PATH` scan -- no subprocess spawn. See this module's doc comment
/// for why a `Command::new(bin).arg("--version")...status()` call used to
/// live here instead and why that was the actual bug.
fn which_uncached(bin: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| is_executable_file(&dir.join(bin)))
}

#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(path: &std::path::Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(path: &Path) {
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).unwrap();
        }
    }

    #[test]
    fn newest_complete_nvm_prefix_is_selected() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("versions/node/v20.0.0/bin");
        let new = root.path().join("versions/node/v22.0.0/bin");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        for dir in [&old, &new] {
            for name in ["node", "npm", "npx"] {
                executable(&dir.join(name));
            }
        }
        // The selector follows Omni's newest-file policy. Make the second
        // prefix observably newer even on filesystems with coarse mtimes.
        std::thread::sleep(Duration::from_millis(20));
        executable(&new.join("node"));
        assert_eq!(newest_nvm_node_bin(root.path()), Some(new));
    }

    #[test]
    fn incomplete_nvm_prefix_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("versions/node/v22.0.0/bin");
        std::fs::create_dir_all(&bin).unwrap();
        executable(&bin.join("node"));
        executable(&bin.join("npm"));
        assert_eq!(newest_nvm_node_bin(root.path()), None);
    }

    #[test]
    fn which_finds_a_real_binary_known_to_exist_in_this_test_environment() {
        // `sh` is POSIX-guaranteed; this environment is Linux-only per
        // the workspace's own CI/dev assumptions.
        assert!(which("sh"), "expected `sh` to be found on PATH");
    }

    #[test]
    fn which_reports_false_for_a_binary_name_that_cannot_plausibly_exist() {
        assert!(!which("acpx-detect-test-nonexistent-binary-xyz123"));
    }

    #[test]
    fn which_is_cached_across_repeated_calls_for_the_same_binary() {
        // Uses a binary name ("node") not touched by this file's other
        // tests, so this test's "cache starts empty" precondition can't
        // race a concurrently-running sibling test that also calls
        // `which("sh")`.
        // Deliberately does not mutate the process-wide `PATH` env var --
        // this test binary (`acpx-core`'s own `src/` unit tests) runs
        // every `mod tests` block in every `src/*.rs` file concurrently
        // on separate threads within one process, including `router.rs`'s
        // tests that spawn a real `sh` stand-in backend needing to
        // resolve `sh` via that same global `PATH`. Mutating it here
        // would race those, exactly the hazard
        // `acpx-core/tests/gateway_native_coverage_test.rs`'s module doc
        // comment already documents and works around with a whole-file
        // serialization lock for its own (separate binary, so unaffected
        // by this one) `PATH` mutations. Instead, this inspects the
        // private cache directly via `cache_entry_for_test`, which proves
        // caching happened without touching any process-global state.
        assert!(
            cache_entry_for_test("node").is_none(),
            "test bug: another test in this binary already cached \"node\""
        );
        assert!(
            which("node"),
            "expected \"node\" to be found on PATH in this dev environment"
        );
        let (cached_found, cached_since) =
            cache_entry_for_test("node").expect("which(\"node\") must populate the cache");
        assert!(cached_found, "cached result for \"node\" must be true");
        assert!(
            cached_since.elapsed() < WHICH_CACHE_TTL,
            "freshly-written cache entry must not already be expired"
        );
    }
}
