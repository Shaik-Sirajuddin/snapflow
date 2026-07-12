//! Daemon startup config: bind addr, default profile, backend spawn spec.

use acpx_conductor::SpawnSpec;

/// Which backend to proxy to by default (native/unmanaged mode, no
/// `_acpx.profile` given -- see `02-architecture.md`), and how to spawn
/// it. Phase 3 adds real profile -> agent/provider resolution on top of
/// this single default; until then `default_agent_id` is the only agent
/// `Router` knows how to spawn.
pub struct ServerConfig {
    pub default_agent_id: String,
    pub backend: SpawnSpec,
    /// Bind address for the HTTP/WS transport (Phase 2 step 11).
    pub http_bind_addr: std::net::SocketAddr,
}

impl ServerConfig {
    /// Read the backend command from `ACPX_BACKEND_CMD` (space-separated
    /// program + args), defaulting to `codex-acp` via npx per the official
    /// registry (see `01-research.md`) if unset. `ACPX_HTTP_BIND` sets the
    /// HTTP/WS bind address (default `127.0.0.1:8790` -- loopback only,
    /// per `05-open-risks.md`'s unresolved transport-security note; do not
    /// point this at a public interface without adding auth/TLS first).
    pub fn from_env() -> Self {
        let raw = std::env::var("ACPX_BACKEND_CMD")
            .unwrap_or_else(|_| "npx -y @agentclientprotocol/codex-acp@1.1.2".to_string());
        let mut parts = raw.split_whitespace();
        let program = parts.next().unwrap_or("npx").to_string();
        let args: Vec<String> = parts.map(|s| s.to_string()).collect();
        let default_agent_id =
            std::env::var("ACPX_DEFAULT_AGENT_ID").unwrap_or_else(|_| "default".to_string());
        let http_bind_addr = std::env::var("ACPX_HTTP_BIND")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| ([127, 0, 0, 1], 8790).into());
        Self {
            default_agent_id,
            backend: SpawnSpec::new(program, args),
            http_bind_addr,
        }
    }
}
