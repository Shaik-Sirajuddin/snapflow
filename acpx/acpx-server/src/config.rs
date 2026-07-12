//! Daemon startup config: bind addr, default profile, backend spawn spec
//! for the Phase 1 spike, etc.

use acpx_conductor::SpawnSpec;

/// Phase 1 config: which single backend to proxy to, and how to spawn it.
/// Later phases replace this with registry-driven, per-agent spawn specs
/// (see `acpx-registry`) selected by profile.
pub struct ServerConfig {
    pub backend: SpawnSpec,
}

impl ServerConfig {
    /// Read the backend command from `ACPX_BACKEND_CMD` (space-separated
    /// program + args), defaulting to `codex-acp` via npx per the official
    /// registry (see `01-research.md`) if unset.
    pub fn from_env() -> Self {
        let raw = std::env::var("ACPX_BACKEND_CMD")
            .unwrap_or_else(|_| "npx -y @agentclientprotocol/codex-acp@1.1.2".to_string());
        let mut parts = raw.split_whitespace();
        let program = parts.next().unwrap_or("npx").to_string();
        let args: Vec<String> = parts.map(|s| s.to_string()).collect();
        Self {
            backend: SpawnSpec::new(program, args),
        }
    }
}
