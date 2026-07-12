//! Agent auto-detection: per registry entry, checks whether its
//! distribution method's runtime is available. Phase 2 step 6.

use acpx_proto::agent::AgentStatus;
use acpx_registry::Distribution;
use std::process::Command;

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
    Command::new(bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
