//! Daemon startup config: bind addr, default profile, backend spawn spec.

use acpx_conductor::SpawnSpec;
use acpx_core::LifecycleConfig;

/// Which backend to proxy to by default (native/unmanaged mode, no
/// `_acpx.profile` given -- see `02-architecture.md`), and how to spawn
/// it. Phase 3 adds real profile -> agent/provider resolution on top of
/// this single default; until then `default_agent_id` is the only agent
/// `Router` knows how to spawn.
pub struct ServerConfig {
    pub default_agent_id: String,
    pub backend: SpawnSpec,
    /// Optional strict-ACP `/acp` bridge policy. `None` keeps every bridge
    /// route disabled so legacy ACPX deployments retain their exact public
    /// surface until an operator opts in.
    pub bridge: Option<acpx_bridge::BridgeConfig>,
    /// Bind address for the HTTP/WS transport (Phase 2 step 11). `None`
    /// means the transport is disabled outright (`ACPX_HTTP_BIND=off`/
    /// `none` -- see `from_env`'s doc comment for why a
    /// per-conversation-subprocess ACP client, e.g. OpenHands's
    /// `ACPAgent`, wants this).
    pub http_bind_addr: Option<std::net::SocketAddr>,
    /// Optional bearer token gating `POST /rpc` and the `GET /ws` upgrade
    /// (Phase 2/post-review "auth" hardening -- see `transport::http`'s
    /// `AuthConfig`). `None` (the default, `ACPX_AUTH_TOKEN` unset) keeps
    /// this transport fully unauthenticated, matching every pre-existing
    /// deployment/test that never set this var.
    pub auth_token: Option<String>,
    /// Whether durable open sessions are proactively restored before any
    /// transport starts. Defaults to enabled only when `ACPX_DB_PATH` is
    /// set; `ACPX_STARTUP_SESSION_RECOVERY_ENABLED=0` disables it.
    pub startup_session_recovery_enabled: bool,
    /// Runs native session retention cleanup independently of transport
    /// connection lifetime. Set `ACPX_LIFECYCLE_REAPER_ENABLED=0` to
    /// disable, primarily for controlled diagnostics.
    pub lifecycle_reaper_enabled: bool,
    /// Poll interval for the lifecycle reaper. The actual session TTLs live
    /// in `lifecycle`; this only controls observation lag.
    pub lifecycle_reaper_interval: std::time::Duration,
    /// Native session admission and retention policy. Configure with:
    /// `ACPX_MAX_SESSIONS_TOTAL`, `ACPX_MAX_SESSIONS_PER_TENANT`,
    /// `ACPX_SESSION_IDLE_TTL_SECONDS`,
    /// `ACPX_UNBOUND_BRIDGE_SESSION_TTL_SECONDS`, and optionally
    /// `ACPX_SESSION_ABSOLUTE_TTL_SECONDS` (`off` disables it).
    pub lifecycle: LifecycleConfig,
    /// Maximum concurrent persistent stdio/WS subscribers for one
    /// tenant-scoped gateway session.
    pub max_subscribers_per_session: usize,
    /// Number of session updates retained per session for resumable
    /// persistent transport subscriptions.
    pub stream_replay_buffer_size: usize,
    /// Grace period before an inactive stream's replay state is removed.
    pub stream_idle_retention: std::time::Duration,
    /// Give each tenant using one managed profile an isolated backend
    /// process. Defaults to the legacy shared-profile behavior.
    pub tenant_process_isolation: bool,
}

impl ServerConfig {
    /// Read the backend command from `ACPX_BACKEND_CMD` (space-separated
    /// program + args), defaulting to `codex-acp` via npx per the official
    /// registry (see `01-research.md`) if unset. `ACPX_HTTP_BIND` sets the
    /// HTTP/WS bind address (default `127.0.0.1:8790` -- loopback only,
    /// per `05-open-risks.md`'s unresolved transport-security note; do not
    /// point this at a public interface without adding auth/TLS first).
    /// `ACPX_HTTP_BIND=off` (or `none`, case-insensitive) disables the
    /// HTTP/WS transport entirely -- the shape any ACP client that spawns
    /// `acpx-server` itself as a per-conversation stdio subprocess (the
    /// backward-compatible path documented in `main.rs`'s module doc
    /// comment; OpenHands's `ACPAgent`/`ACPAgentSettings` is exactly this
    /// shape) wants, since it never talks to the HTTP/WS surface at all
    /// and a second/third concurrent instance on the same host would
    /// otherwise contend for one fixed default port for no reason. Even
    /// without this set, a bind failure at startup (e.g. the default
    /// port already in use by another instance) is treated as
    /// *non-fatal* to the stdio transport -- see `main.rs`'s startup
    /// sequence -- so a client that only cares about stdio ACP semantics
    /// still gets a fully working subprocess either way; this explicit
    /// opt-out just skips the doomed bind attempt (and its warning log)
    /// outright when the caller already knows HTTP/WS is unwanted.
    /// `ACPX_AUTH_TOKEN`, if set, requires every HTTP/WS client to present
    /// it as `Authorization: Bearer <token>` -- still no TLS provided by
    /// this process itself, so pair this with a TLS-terminating reverse
    /// proxy for any non-loopback bind address.
    pub fn from_env() -> Self {
        let raw = std::env::var("ACPX_BACKEND_CMD")
            .unwrap_or_else(|_| "npx -y @agentclientprotocol/codex-acp@1.1.2".to_string());
        let mut parts = raw.split_whitespace();
        let program = parts.next().unwrap_or("npx").to_string();
        let args: Vec<String> = parts.map(|s| s.to_string()).collect();
        let default_agent_id =
            std::env::var("ACPX_DEFAULT_AGENT_ID").unwrap_or_else(|_| "default".to_string());
        let http_bind_addr = match std::env::var("ACPX_HTTP_BIND") {
            Ok(raw) if raw.eq_ignore_ascii_case("off") || raw.eq_ignore_ascii_case("none") => None,
            Ok(raw) => Some(raw.parse().unwrap_or_else(|err| {
                panic!("ACPX_HTTP_BIND={raw:?} is not a valid socket address: {err}")
            })),
            Err(_) => Some(([127, 0, 0, 1], 8790).into()),
        };
        // Treat an empty string the same as unset -- an operator who sets
        // `ACPX_AUTH_TOKEN=""` (e.g. via a templated env file with an
        // unfilled placeholder) almost certainly meant "no auth", not "the
        // empty string is the secret token", and requiring clients to
        // send `Authorization: Bearer ` with nothing after it would be a
        // confusing footgun.
        let auth_token = std::env::var("ACPX_AUTH_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        let startup_session_recovery_enabled =
            match std::env::var("ACPX_STARTUP_SESSION_RECOVERY_ENABLED") {
                Ok(value) => value != "0",
                Err(_) => std::env::var_os("ACPX_DB_PATH").is_some(),
            };
        let lifecycle_reaper_enabled = std::env::var("ACPX_LIFECYCLE_REAPER_ENABLED")
            .map(|value| value != "0")
            .unwrap_or(true);
        let lifecycle_reaper_interval = std::env::var("ACPX_LIFECYCLE_REAPER_INTERVAL_SECONDS")
            .ok()
            .map(|value| {
                let seconds = value.parse::<u64>().unwrap_or_else(|err| {
                    panic!(
                        "ACPX_LIFECYCLE_REAPER_INTERVAL_SECONDS={value:?} is not a positive integer: {err}"
                    )
                });
                assert!(
                    seconds > 0,
                    "ACPX_LIFECYCLE_REAPER_INTERVAL_SECONDS must be greater than zero"
                );
                std::time::Duration::from_secs(seconds)
            })
            .unwrap_or_else(|| std::time::Duration::from_secs(60));
        let mut lifecycle = LifecycleConfig::default();
        lifecycle.max_sessions_total =
            positive_usize("ACPX_MAX_SESSIONS_TOTAL", lifecycle.max_sessions_total);
        lifecycle.max_sessions_per_tenant = positive_usize(
            "ACPX_MAX_SESSIONS_PER_TENANT",
            lifecycle.max_sessions_per_tenant,
        );
        lifecycle.idle_session_ttl =
            positive_duration("ACPX_SESSION_IDLE_TTL_SECONDS", lifecycle.idle_session_ttl);
        lifecycle.unbound_bridge_session_ttl = positive_duration(
            "ACPX_UNBOUND_BRIDGE_SESSION_TTL_SECONDS",
            lifecycle.unbound_bridge_session_ttl,
        );
        lifecycle.absolute_session_ttl = match std::env::var("ACPX_SESSION_ABSOLUTE_TTL_SECONDS") {
            Ok(value)
                if value.eq_ignore_ascii_case("off") || value.eq_ignore_ascii_case("none") =>
            {
                None
            }
            Ok(value) => Some(parse_positive_duration(
                "ACPX_SESSION_ABSOLUTE_TTL_SECONDS",
                &value,
            )),
            Err(_) => lifecycle.absolute_session_ttl,
        };
        lifecycle
            .validate()
            .unwrap_or_else(|err| panic!("invalid ACPX lifecycle configuration: {err}"));
        let max_subscribers_per_session = positive_usize("ACPX_MAX_SUBSCRIBERS_PER_SESSION", 16);
        let stream_replay_buffer_size = positive_usize("ACPX_STREAM_REPLAY_BUFFER_SIZE", 200);
        let stream_idle_retention = positive_duration(
            "ACPX_STREAM_IDLE_RETENTION_SECS",
            std::time::Duration::from_secs(300),
        );
        let tenant_process_isolation = std::env::var("ACPX_TENANT_PROCESS_ISOLATION")
            .map(|value| value == "1")
            .unwrap_or(false);
        let bridge = acpx_bridge::BridgeConfig::from_env()
            .unwrap_or_else(|err| panic!("invalid ACP bridge configuration: {err}"));
        Self {
            default_agent_id,
            backend: SpawnSpec::new(program, args),
            bridge,
            http_bind_addr,
            auth_token,
            startup_session_recovery_enabled,
            lifecycle_reaper_enabled,
            lifecycle_reaper_interval,
            lifecycle,
            max_subscribers_per_session,
            stream_replay_buffer_size,
            stream_idle_retention,
            tenant_process_isolation,
        }
    }
}

fn positive_usize(name: &str, default: usize) -> usize {
    let parsed = match std::env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .unwrap_or_else(|err| panic!("{name}={value:?} is not a positive integer: {err}")),
        Err(_) => default,
    };
    assert!(parsed > 0, "{name} must be greater than zero");
    parsed
}

fn positive_duration(name: &str, default: std::time::Duration) -> std::time::Duration {
    match std::env::var(name) {
        Ok(value) => parse_positive_duration(name, &value),
        Err(_) => default,
    }
}

fn parse_positive_duration(name: &str, value: &str) -> std::time::Duration {
    let seconds = value
        .parse::<u64>()
        .unwrap_or_else(|err| panic!("{name}={value:?} is not a positive integer: {err}"));
    assert!(seconds > 0, "{name} must be greater than zero");
    std::time::Duration::from_secs(seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_positive_lifecycle_duration() {
        assert_eq!(
            parse_positive_duration("ACPX_SESSION_IDLE_TTL_SECONDS", "42"),
            std::time::Duration::from_secs(42)
        );
    }

    #[test]
    #[should_panic(expected = "must be greater than zero")]
    fn rejects_zero_lifecycle_duration() {
        parse_positive_duration("ACPX_SESSION_IDLE_TTL_SECONDS", "0");
    }

    #[test]
    #[should_panic(expected = "must be greater than zero")]
    fn rejects_zero_subscriber_limit() {
        positive_usize("ACPX_MAX_SUBSCRIBERS_PER_SESSION", 0);
    }

    #[test]
    #[should_panic(expected = "must be greater than zero")]
    fn rejects_zero_replay_buffer_size() {
        positive_usize("ACPX_STREAM_REPLAY_BUFFER_SIZE", 0);
    }

    #[test]
    #[should_panic(expected = "must be greater than zero")]
    fn rejects_zero_stream_idle_retention() {
        parse_positive_duration("ACPX_STREAM_IDLE_RETENTION_SECS", "0");
    }
}
