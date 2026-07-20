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
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a `which()` result is trusted before being re-checked. Kept
/// short (not "forever") so a runtime installed *after* acpx-server
/// started (e.g. `uv` installed mid-session) is picked up without a
/// restart, just not on literally the next request.
const WHICH_CACHE_TTL: Duration = Duration::from_secs(300);

/// Best-effort detection for a single registry entry's preferred
/// distribution method. `npx`/`uvx` entries: checks the runtime
/// (`node`+`npm`, or `uv`) is on `PATH` -- the runtime itself resolves the
/// package on demand, so there's no separate "package installed" check.
/// `binary` entries: checks `~/.acpx/adapters/<id>/` for an already-fetched
/// copy (Phase 4 fills in the actual fetch step).
pub fn detect(agent_id: &str, dist: &Distribution) -> AgentStatus {
    match dist.preferred_method() {
        Some("npx") => {
            if which("node") && which("npm") {
                AgentStatus::Installed
            } else {
                AgentStatus::RuntimeMissing
            }
        }
        Some("uvx") => {
            if which("uv") {
                AgentStatus::Installed
            } else {
                AgentStatus::RuntimeMissing
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
    dirs_home().join(".acpx").join("adapters")
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
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
        assert!(which("node"), "expected \"node\" to be found on PATH in this dev environment");
        let (cached_found, cached_since) =
            cache_entry_for_test("node").expect("which(\"node\") must populate the cache");
        assert!(cached_found, "cached result for \"node\" must be true");
        assert!(
            cached_since.elapsed() < WHICH_CACHE_TTL,
            "freshly-written cache entry must not already be expired"
        );
    }
}
