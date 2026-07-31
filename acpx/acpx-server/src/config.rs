//! Daemon startup config: bind addr, default profile, backend spawn spec.

use acpx_conductor::SpawnSpec;
use acpx_core::LifecycleConfig;

/// Which agent command to proxy to by default (native/unmanaged mode, no
/// `_acpx.profile` given -- see `02-architecture.md`), and how to spawn
/// it. Profiles can override this per `session/new`. Phase 3 adds real
/// profile -> agent/provider resolution on top of this single default;
/// until then `default_agent_id` is the only agent `Router` registers at
/// startup (additional agents still register on demand via profiles).
pub struct ServerConfig {
    pub default_agent_id: String,
    /// Backend auth method for native/unmanaged sessions. This is opt-in so
    /// native ACPX retains its no-guessing authentication default.
    pub native_auth_method_id: Option<String>,
    /// Native-mode default ACP agent command (`ACPX_DEFAULT_ACP_COMMAND`).
    /// Profiles / `_acpx.agentId` override this for managed sessions.
    pub default_acp_command: SpawnSpec,
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
    /// Optional tenant-bound bearer tokens (`ACPX_AUTH_TENANT_TOKENS`,
    /// format `tenant_id=token,tenant_id=token,...`) -- see
    /// `transport::http`'s "Identity-bound tenant auth" doc comment.
    /// Empty by default, which keeps every pre-existing deployment's
    /// `X-Acpx-Tenant` header fully self-declared/cooperative, unchanged.
    /// Additive to `auth_token`: a request may authenticate via either.
    pub auth_tenant_tokens: Vec<(String, acpx_core::TenantId)>,
    /// Optional tenant namespace allowlist (`ACPX_TENANT_ALLOWLIST`,
    /// comma-separated tenant ids). `None` (the default) keeps every
    /// pre-existing deployment's "any caller-declared/authenticated
    /// tenant string is a valid namespace" behavior unchanged.
    pub auth_tenant_allowlist: Option<std::collections::HashSet<String>>,
    /// Optional bearer token for the loopback-only admin HTTP surface.
    /// Unlike `auth_token`, enabling this requires `ACPX_DB_PATH` so
    /// gateway-wide agent administration is always durable.
    pub admin_token: Option<String>,
    /// Loopback bind address for `/admin/*`. Present only when
    /// `ACPX_ADMIN_TOKEN` is configured.
    pub admin_bind_addr: Option<std::net::SocketAddr>,
    /// Whether durable open sessions are proactively restored before any
    /// transport starts. Defaults to enabled only when `ACPX_DB_PATH` is
    /// set; `ACPX_STARTUP_SESSION_RECOVERY_ENABLED=0` disables it.
    pub startup_session_recovery_enabled: bool,
    /// Deadline for one adapter-native session restore during startup.
    pub startup_session_recovery_timeout: std::time::Duration,
    /// Maximum simultaneous recovery jobs. Different connectors can run in
    /// parallel; requests to one connector remain stdio-serialized.
    pub startup_session_recovery_concurrency: usize,
    /// Stop startup after the first failed recovery rather than serving a
    /// partially restored daemon.
    pub startup_session_recovery_fail_fast: bool,
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
    /// Give each managed session its own dedicated backend process
    /// instead of sharing one process per profile[/tenant] with every
    /// other session using it (`backend_process_model` hardening item,
    /// `acp-gateway-daemon` plan). Composable with
    /// `tenant_process_isolation`. Defaults to the legacy shared-process
    /// behavior; native/unmanaged sessions are unaffected either way.
    pub session_process_isolation: bool,
    /// **`process_reader_demux`, phase 1 of
    /// `memory/acpx/gen/acpx-concurrency-config-execution.meta.json`.**
    /// **On by default** (`ACPX_PROCESS_READER_DEMUX=0` opts out -- see
    /// `Router::process_reader_demux`'s field doc comment for the three
    /// regressions that had to be closed, each with its own regression
    /// test, before this default could safely flip). When on,
    /// `session/prompt`/`session/new` register-then-await a backend
    /// response via a per-process reader task instead of holding that
    /// process's own lock across the entire write + blocking-read-loop
    /// of a turn -- so two sessions sharing one backend process (the
    /// live default when both isolation flags above are also off) can
    /// actually overlap in wall time. See `Router::process_reader_demux`'s
    /// field doc comment for the current scope and known tradeoffs (now
    /// covers `session/fork` and the real `session/list` path too, not
    /// just `session/prompt`/`session/new`).
    pub process_reader_demux: bool,
}

impl ServerConfig {
    /// Read the native-mode default ACP command from
    /// `ACPX_DEFAULT_ACP_COMMAND` (space-separated program + args),
    /// defaulting to `codex-acp` via npx per the official registry (see
    /// `01-research.md`) if unset. Legacy alias: `ACPX_BACKEND_CMD` is still
    /// accepted when the new name is unset. `ACPX_HTTP_BIND` sets the
    /// HTTP/WS bind address (default = `acpx_proto::DEFAULT_ACPX_HTTP_ADDR`,
    /// the single source of truth panel-rust also dials, currently
    /// `127.0.0.1:8790` -- loopback only,
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
    /// `ACPX_AUTH_TENANT_TOKENS` (`tenant_id=token,tenant_id=token,...`)
    /// additionally configures identity-bound tenant tokens: a request
    /// authenticating with one of these derives its tenant from the
    /// token itself rather than the self-declared `X-Acpx-Tenant`
    /// header, and is rejected if that header names a different tenant
    /// -- see `transport::http`'s "Identity-bound tenant auth" doc
    /// comment for the full contract.
    /// `ACPX_TENANT_ALLOWLIST` (comma-separated tenant ids) additionally
    /// rejects (`403`) any resolved tenant -- self-declared or
    /// authenticated -- outside this fixed set, closing the
    /// `tenant_namespace_governance` hardening item's "unbounded
    /// tenant-map growth" concern for deployments that know their full
    /// tenant set up front.
    pub fn from_env() -> Self {
        let default_agent_id =
            std::env::var("ACPX_DEFAULT_AGENT_ID").unwrap_or_else(|_| "default".to_string());
        // Explicit env (new name or legacy alias) always wins. When unset,
        // PROF-14: prefer a single real registry agent's launch command
        // (matching ACPX_DEFAULT_AGENT_ID if it names a registry id, else
        // exactly one Installed launchable agent) over the hardcoded
        // codex-acp npx literal. Ambiguous/empty still falls back.
        let default_acp_command = match std::env::var("ACPX_DEFAULT_ACP_COMMAND")
            .or_else(|_| std::env::var("ACPX_BACKEND_CMD"))
        {
            Ok(raw) => {
                let mut parts = raw.split_whitespace();
                let program = parts.next().unwrap_or("npx").to_string();
                let args: Vec<String> = parts.map(|s| s.to_string()).collect();
                SpawnSpec::new(program, args)
            }
            Err(_) => resolve_native_default_from_seeded_registry(&default_agent_id)
                .unwrap_or_else(|| {
                    SpawnSpec::new(
                        "npx",
                        vec![
                            "-y".to_string(),
                            "@agentclientprotocol/codex-acp@1.1.2".to_string(),
                        ],
                    )
                }),
        };
        let program = default_acp_command.program.clone();
        let args = default_acp_command.args.clone();
        let native_auth_method_id = std::env::var("ACPX_NATIVE_AUTH_METHOD_ID")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| default_codex_native_auth_method(&program, &args));
        let http_bind_addr = match std::env::var("ACPX_HTTP_BIND") {
            Ok(raw) if raw.eq_ignore_ascii_case("off") || raw.eq_ignore_ascii_case("none") => None,
            Ok(raw) => Some(raw.parse().unwrap_or_else(|err| {
                panic!("ACPX_HTTP_BIND={raw:?} is not a valid socket address: {err}")
            })),
            Err(_) => Some(
                acpx_proto::DEFAULT_ACPX_HTTP_ADDR
                    .parse()
                    .expect("DEFAULT_ACPX_HTTP_ADDR must be a valid socket address"),
            ),
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
        // `tenant_id=token` pairs, comma-separated. Whitespace around
        // each pair/field is trimmed for forgiving shell/env-file
        // authoring; a malformed entry (missing `=`, empty tenant id, or
        // empty token) panics at startup rather than silently dropping
        // or misparsing a security-relevant mapping.
        let auth_tenant_tokens: Vec<(String, acpx_core::TenantId)> =
            match std::env::var("ACPX_AUTH_TENANT_TOKENS") {
                Ok(raw) if !raw.trim().is_empty() => raw
                    .split(',')
                    .map(|entry| {
                        let entry = entry.trim();
                        let (tenant, token) = entry.split_once('=').unwrap_or_else(|| {
                            panic!(
                                "ACPX_AUTH_TENANT_TOKENS entry {entry:?} must be `tenant_id=token`"
                            )
                        });
                        let (tenant, token) = (tenant.trim(), token.trim());
                        assert!(
                            !tenant.is_empty(),
                            "ACPX_AUTH_TENANT_TOKENS entry {entry:?} has an empty tenant id"
                        );
                        assert!(
                            !token.is_empty(),
                            "ACPX_AUTH_TENANT_TOKENS entry {entry:?} has an empty token"
                        );
                        (token.to_string(), acpx_core::TenantId::from(tenant))
                    })
                    .collect(),
                _ => Vec::new(),
            };
        let auth_tenant_allowlist: Option<std::collections::HashSet<String>> =
            match std::env::var("ACPX_TENANT_ALLOWLIST") {
                Ok(raw) if !raw.trim().is_empty() => Some(
                    raw.split(',')
                        .map(|entry| entry.trim().to_string())
                        .filter(|entry| !entry.is_empty())
                        .collect(),
                ),
                _ => None,
            };
        let admin_token = std::env::var("ACPX_ADMIN_TOKEN")
            .ok()
            .filter(|t| !t.is_empty());
        let admin_bind_addr = admin_token.as_ref().map(|_| {
            let raw =
                std::env::var("ACPX_ADMIN_BIND").unwrap_or_else(|_| "127.0.0.1:8791".to_owned());
            let address = raw.parse::<std::net::SocketAddr>().unwrap_or_else(|err| {
                panic!("ACPX_ADMIN_BIND={raw:?} is not a valid socket address: {err}")
            });
            assert!(
                address.ip().is_loopback(),
                "ACPX_ADMIN_BIND must use a loopback address"
            );
            address
        });
        let startup_session_recovery_enabled =
            match std::env::var("ACPX_STARTUP_SESSION_RECOVERY_ENABLED") {
                Ok(value) => value != "0",
                Err(_) => std::env::var_os("ACPX_DB_PATH").is_some(),
            };
        let startup_session_recovery_timeout = positive_duration(
            "ACPX_STARTUP_SESSION_RECOVERY_TIMEOUT_SECONDS",
            std::time::Duration::from_secs(30),
        );
        let startup_session_recovery_concurrency =
            positive_usize("ACPX_STARTUP_SESSION_RECOVERY_CONCURRENCY", 2);
        let startup_session_recovery_fail_fast =
            std::env::var("ACPX_STARTUP_SESSION_RECOVERY_FAIL_FAST")
                .map(|value| value == "1")
                .unwrap_or(false);
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
        lifecycle.max_pinned_sessions_per_tenant =
            match std::env::var("ACPX_MAX_PINNED_SESSIONS_PER_TENANT") {
                Ok(value)
                    if value.eq_ignore_ascii_case("off") || value.eq_ignore_ascii_case("none") =>
                {
                    None
                }
                Ok(value) => Some(value.parse::<usize>().unwrap_or_else(|err| {
                    panic!(
                        "ACPX_MAX_PINNED_SESSIONS_PER_TENANT={value:?} is not a positive \
                             integer: {err}"
                    )
                })),
                Err(_) => lifecycle.max_pinned_sessions_per_tenant,
            };
        lifecycle.connector_idle_shutdown_ttl =
            match std::env::var("ACPX_CONNECTOR_IDLE_SHUTDOWN_TTL_SECONDS") {
                Ok(value)
                    if value.eq_ignore_ascii_case("off") || value.eq_ignore_ascii_case("none") =>
                {
                    None
                }
                Ok(value) => Some(parse_positive_duration(
                    "ACPX_CONNECTOR_IDLE_SHUTDOWN_TTL_SECONDS",
                    &value,
                )),
                Err(_) => lifecycle.connector_idle_shutdown_ttl,
            };
        lifecycle.active_turn_deadline = match std::env::var("ACPX_ACTIVE_TURN_DEADLINE_SECONDS") {
            Ok(value)
                if value.eq_ignore_ascii_case("off") || value.eq_ignore_ascii_case("none") =>
            {
                None
            }
            Ok(value) => Some(parse_positive_duration(
                "ACPX_ACTIVE_TURN_DEADLINE_SECONDS",
                &value,
            )),
            Err(_) => lifecycle.active_turn_deadline,
        };
        lifecycle.background_mode = std::env::var("ACPX_BACKGROUND_MODE")
            .map(|value| value == "1")
            .unwrap_or(lifecycle.background_mode);
        lifecycle.startup_recovery_max_age =
            match std::env::var("ACPX_STARTUP_RECOVERY_MAX_AGE_SECONDS") {
                Ok(value)
                    if value.eq_ignore_ascii_case("off") || value.eq_ignore_ascii_case("none") =>
                {
                    None
                }
                Ok(value) => Some(parse_positive_duration(
                    "ACPX_STARTUP_RECOVERY_MAX_AGE_SECONDS",
                    &value,
                )),
                Err(_) => lifecycle.startup_recovery_max_age,
            };
        lifecycle.max_new_sessions_per_list_call =
            match std::env::var("ACPX_MAX_NEW_SESSIONS_PER_LIST_CALL") {
                Ok(value)
                    if value.eq_ignore_ascii_case("off") || value.eq_ignore_ascii_case("none") =>
                {
                    None
                }
                Ok(value) => Some(value.parse::<usize>().unwrap_or_else(|err| {
                    panic!("ACPX_MAX_NEW_SESSIONS_PER_LIST_CALL={value:?} is not a positive integer: {err}")
                })),
                Err(_) => lifecycle.max_new_sessions_per_list_call,
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
        let session_process_isolation = std::env::var("ACPX_SESSION_PROCESS_ISOLATION")
            .map(|value| value == "1")
            .unwrap_or(false);
        let process_reader_demux = std::env::var("ACPX_PROCESS_READER_DEMUX")
            .map(|value| value == "1")
            .unwrap_or(true);
        let bridge = acpx_bridge::BridgeConfig::from_env()
            .unwrap_or_else(|err| panic!("invalid ACP bridge configuration: {err}"));
        Self {
            default_agent_id,
            native_auth_method_id,
            default_acp_command,
            bridge,
            http_bind_addr,
            auth_token,
            auth_tenant_tokens,
            auth_tenant_allowlist,
            admin_token,
            admin_bind_addr,
            startup_session_recovery_enabled,
            startup_session_recovery_timeout,
            startup_session_recovery_concurrency,
            startup_session_recovery_fail_fast,
            lifecycle_reaper_enabled,
            lifecycle_reaper_interval,
            lifecycle,
            max_subscribers_per_session,
            stream_replay_buffer_size,
            stream_idle_retention,
            tenant_process_isolation,
            session_process_isolation,
            process_reader_demux,
        }
    }
}

/// PROF-14: when neither `ACPX_DEFAULT_ACP_COMMAND` nor legacy
/// `ACPX_BACKEND_CMD` is set, pick a native-mode backend spawn from the
/// ACP registry the same way `Router::ensure_default_profiles_seeded`
/// would auto-seed profiles — but only when the choice is unambiguous.
fn resolve_native_default_from_seeded_registry(default_agent_id: &str) -> Option<SpawnSpec> {
    let registry = acpx_registry::fallback_registry();
    let launchable: Vec<&acpx_registry::Agent> = registry
        .agents
        .iter()
        .filter(|agent| {
            matches!(
                acpx_core::detect::detect(&agent.id, &agent.distribution),
                acpx_proto::agent::AgentStatus::Installed
                    | acpx_proto::agent::AgentStatus::InstalledNoSession
            ) && registry_spawn_spec(agent).is_some()
        })
        .collect();

    if let Some(agent) = launchable.iter().find(|a| a.id == default_agent_id) {
        return registry_spawn_spec(agent);
    }
    if matches!(default_agent_id, "default" | "codex") {
        if let Some(agent) = launchable.iter().find(|a| a.id == "codex-acp") {
            return registry_spawn_spec(agent);
        }
    }
    if launchable.len() == 1 {
        return registry_spawn_spec(launchable[0]);
    }
    None
}

fn registry_spawn_spec(agent: &acpx_registry::Agent) -> Option<SpawnSpec> {
    if let Some(npx) = &agent.distribution.npx {
        let mut args = vec!["-y".to_string(), npx.package.clone()];
        args.extend(npx.args.iter().cloned());
        let program = std::env::var("SNAPFLOW_ACP_NODE_HOME")
            .ok()
            .map(|h| std::path::Path::new(&h).join("bin").join("npx"))
            .filter(|path| path.is_file())
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| "npx".to_string());
        return Some(SpawnSpec::new(program, args));
    }
    if let Some(uvx) = &agent.distribution.uvx {
        let mut args = vec![uvx.package.clone()];
        args.extend(uvx.args.iter().cloned());
        return Some(SpawnSpec::new("uvx", args));
    }
    None
}

/// Auto-defaults `native_auth_method_id` for a codex-acp backend when the
/// operator hasn't set `ACPX_NATIVE_AUTH_METHOD_ID` explicitly, mirroring
/// the same auto-detection panel-rust's own `spawn_gateway_process`
/// already applies to its self-spawned gateway (see `agent_bridge.rs`'s
/// "give it a noninteractive path to this system's already-authenticated
/// Codex CLI login" comment) -- without this, codex-acp falls back to its
/// headless-incapable ChatGPT device login flow and every session fails
/// with "backend requires authentication", even though a real, already-
/// logged-in Codex CLI session is sitting right there in `~/.codex/
/// auth.json`. Doing this here, in acpx-server itself, means every caller
/// that spawns this server directly -- not just panel-rust's own wrapper
/// -- gets a working ambient login for free. Only fires for the codex-acp
/// npx package (never silently reinterprets an operator's custom
/// `ACPX_DEFAULT_ACP_COMMAND` for some other adapter).
///
/// Checks *both* auth methods a real `auth.json` can represent, not just
/// the API-key one: `codex login --api-key` writes a raw `OPENAI_API_KEY`,
/// while the far more common `codex login` (ChatGPT-plan device flow)
/// writes a `tokens.access_token` OAuth session and leaves `OPENAI_API_KEY`
/// null. Before this, only the API-key case was auto-detected -- an
/// operator with a real, valid, already-authenticated ChatGPT-plan login
/// (no raw key ever stored, by design) got the exact same "backend
/// requires authentication" failure as someone with no login at all,
/// confirmed live: acpx-server logged `native_auth_method_id=None` at
/// startup against a real `auth.json` with a live `tokens.access_token`
/// and zero `OPENAI_API_KEY`, and every `session/new` failed until
/// `ACPX_NATIVE_AUTH_METHOD_ID=chat-gpt` was set by hand. Prefers the API
/// key when (unusually) both are present, since it needs no interactive
/// device-flow confirmation of any kind.
fn default_codex_native_auth_method(program: &str, args: &[String]) -> Option<String> {
    let is_codex_acp = program == "npx"
        && args
            .iter()
            .any(|arg| arg.starts_with("@agentclientprotocol/codex-acp"));
    if !is_codex_acp {
        return None;
    }
    let auth = read_codex_auth_file()?;
    if let Some(key) = auth
        .get("OPENAI_API_KEY")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        if std::env::var_os("CODEX_API_KEY").is_none() {
            // SAFETY: single-threaded startup, before any backend is spawned;
            // `acpx_conductor::SpawnSpec`'s child processes inherit the
            // ambient environment on top of their own explicit `env` map, so
            // setting it here is enough for every future spawn to see it.
            unsafe {
                std::env::set_var("CODEX_API_KEY", key);
            }
        }
        return Some("api-key".to_string());
    }
    let has_chatgpt_login = auth
        .get("tokens")
        .and_then(|tokens| tokens.get("access_token"))
        .and_then(|token| token.as_str())
        .is_some_and(|token| !token.is_empty());
    if has_chatgpt_login {
        return Some("chat-gpt".to_string());
    }
    None
}

/// Reads and parses the real Codex CLI's own `auth.json` --
/// `ACPX_CODEX_AUTH_FILE` overrides the path outright (same override
/// panel-rust's `codex_home_dir` respects), otherwise `$HOME/.codex/
/// auth.json`. Shared by `default_codex_native_auth_method`'s two auth-
/// method checks (`OPENAI_API_KEY` and `tokens.access_token`) so both
/// read the same file/parse once rather than duplicating path resolution.
fn read_codex_auth_file() -> Option<serde_json::Value> {
    let path = std::env::var_os("ACPX_CODEX_AUTH_FILE")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                std::path::PathBuf::from(home)
                    .join(".codex")
                    .join("auth.json")
            })
        })?;
    let contents = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
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

    /// Both `default_codex_native_auth_method_*` tests below mutate the
    /// same process-global `CODEX_API_KEY`/`ACPX_CODEX_AUTH_FILE` env vars;
    /// `cargo test` runs tests on multiple threads by default, so without
    /// serializing them a genuine data race is possible -- e.g. one test's
    /// guard restoring/removing `ACPX_CODEX_AUTH_FILE` at `Drop` while the
    /// other test's `read_codex_auth_file` call is in flight,
    /// which then falls through to the real `$HOME/.codex/auth.json`
    /// instead of the test's own fake one (observed in practice: the
    /// "finds a real key" test's assert failed with this machine's actual
    /// ambient Codex API key rather than the fixture's `"sk-test-key"`).
    static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Holds `ENV_TEST_LOCK` for its whole lifetime (serializing this test
    /// against the other `default_codex_native_auth_method_*` test) and,
    /// on `Drop`, restores mutated process-global env vars and removes a
    /// temp dir -- so a panicking `assert!` mid-test still runs cleanup
    /// instead of leaking the temp dir and leaving the env corrupted for
    /// whichever test runs next. Mirrors `startup_recovery_test.rs`'s
    /// `BinaryGuard` Drop-cleanup shape.
    struct EnvRestoreGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        vars: Vec<(&'static str, Option<String>)>,
        temp_dir: std::path::PathBuf,
    }

    impl EnvRestoreGuard {
        /// Acquires `ENV_TEST_LOCK` *before* snapshotting `vars`' current
        /// values, so the snapshot can't itself race with the other
        /// test's mutation -- only the value from either true ambient
        /// state or the other test's own already-completed (lock
        /// released) restore is ever captured as "prior".
        fn new(vars: &[&'static str], temp_dir: std::path::PathBuf) -> Self {
            let lock = ENV_TEST_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let vars = vars
                .iter()
                .map(|&key| (key, std::env::var(key).ok()))
                .collect();
            Self {
                _lock: lock,
                vars,
                temp_dir,
            }
        }
    }

    impl Drop for EnvRestoreGuard {
        fn drop(&mut self) {
            unsafe {
                for (key, prior) in &self.vars {
                    match prior {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&self.temp_dir);
        }
    }

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
    fn default_codex_native_auth_method_finds_a_real_key_and_sets_codex_api_key() {
        let dir = std::env::temp_dir().join(format!(
            "acpx-server-codex-auth-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let auth_file = dir.join("auth.json");
        std::fs::write(&auth_file, r#"{"OPENAI_API_KEY": "sk-test-key"}"#)
            .expect("write temp auth file");

        let _guard = EnvRestoreGuard::new(&["ACPX_CODEX_AUTH_FILE", "CODEX_API_KEY"], dir.clone());
        unsafe {
            std::env::set_var("ACPX_CODEX_AUTH_FILE", &auth_file);
            std::env::remove_var("CODEX_API_KEY");
        }

        let result = default_codex_native_auth_method(
            "npx",
            &[
                "-y".to_string(),
                "@agentclientprotocol/codex-acp@1.1.2".to_string(),
            ],
        );

        assert_eq!(result.as_deref(), Some("api-key"));
        assert_eq!(std::env::var("CODEX_API_KEY").as_deref(), Ok("sk-test-key"));
    }

    #[test]
    fn default_codex_native_auth_method_is_a_no_op_for_a_non_codex_backend() {
        let dir = std::env::temp_dir().join(format!(
            "acpx-server-codex-auth-test-nonop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let auth_file = dir.join("auth.json");
        std::fs::write(&auth_file, r#"{"OPENAI_API_KEY": "sk-test-key"}"#)
            .expect("write temp auth file");

        let _guard = EnvRestoreGuard::new(&["ACPX_CODEX_AUTH_FILE"], dir.clone());
        unsafe {
            std::env::set_var("ACPX_CODEX_AUTH_FILE", &auth_file);
        }

        // A custom backend command (e.g. an operator's own claude-agent-acp
        // pin, or a test stand-in binary) must never be silently
        // reinterpreted as codex-acp just because a codex auth file
        // happens to exist on this machine.
        let result = default_codex_native_auth_method("sh", &["./stand-in-agent.sh".to_string()]);
        assert_eq!(result, None);
    }

    #[test]
    fn default_codex_native_auth_method_detects_a_chatgpt_login_when_no_api_key_present() {
        // Live-confirmed regression: `codex login`'s ChatGPT-plan device
        // flow writes a `tokens.access_token` OAuth session and leaves
        // `OPENAI_API_KEY` null -- before this fix, that shape fell
        // through to `None` here just like "no login at all", so a real,
        // valid, already-authenticated Codex CLI login still got
        // "backend requires authentication" on every `session/new`.
        let dir = std::env::temp_dir().join(format!(
            "acpx-server-codex-auth-test-chatgpt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let auth_file = dir.join("auth.json");
        std::fs::write(
            &auth_file,
            r#"{"OPENAI_API_KEY": null, "tokens": {"access_token": "test-access-token", "refresh_token": "test-refresh-token"}}"#,
        )
        .expect("write temp auth file");

        let _guard = EnvRestoreGuard::new(&["ACPX_CODEX_AUTH_FILE", "CODEX_API_KEY"], dir.clone());
        unsafe {
            std::env::set_var("ACPX_CODEX_AUTH_FILE", &auth_file);
            std::env::remove_var("CODEX_API_KEY");
        }

        let result = default_codex_native_auth_method(
            "npx",
            &[
                "-y".to_string(),
                "@agentclientprotocol/codex-acp@1.1.2".to_string(),
            ],
        );

        assert_eq!(result.as_deref(), Some("chat-gpt"));
        // No API key exists in this fixture, so nothing should be set --
        // unlike the API-key path, a ChatGPT login needs no env var, only
        // the resolved `auth_method_id` itself.
        assert!(std::env::var("CODEX_API_KEY").is_err());
    }

    #[test]
    fn default_codex_native_auth_method_prefers_api_key_over_chatgpt_login_when_both_present() {
        let dir = std::env::temp_dir().join(format!(
            "acpx-server-codex-auth-test-both-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let auth_file = dir.join("auth.json");
        std::fs::write(
            &auth_file,
            r#"{"OPENAI_API_KEY": "sk-test-key", "tokens": {"access_token": "test-access-token"}}"#,
        )
        .expect("write temp auth file");

        let _guard = EnvRestoreGuard::new(&["ACPX_CODEX_AUTH_FILE", "CODEX_API_KEY"], dir.clone());
        unsafe {
            std::env::set_var("ACPX_CODEX_AUTH_FILE", &auth_file);
            std::env::remove_var("CODEX_API_KEY");
        }

        let result = default_codex_native_auth_method(
            "npx",
            &[
                "-y".to_string(),
                "@agentclientprotocol/codex-acp@1.1.2".to_string(),
            ],
        );

        assert_eq!(result.as_deref(), Some("api-key"));
        assert_eq!(std::env::var("CODEX_API_KEY").as_deref(), Ok("sk-test-key"));
    }

    #[test]
    fn default_codex_native_auth_method_is_none_when_the_auth_file_has_neither_method() {
        let dir = std::env::temp_dir().join(format!(
            "acpx-server-codex-auth-test-neither-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let auth_file = dir.join("auth.json");
        std::fs::write(&auth_file, r#"{"OPENAI_API_KEY": null}"#).expect("write temp auth file");

        let _guard = EnvRestoreGuard::new(&["ACPX_CODEX_AUTH_FILE", "CODEX_API_KEY"], dir.clone());
        unsafe {
            std::env::set_var("ACPX_CODEX_AUTH_FILE", &auth_file);
        }

        let result = default_codex_native_auth_method(
            "npx",
            &[
                "-y".to_string(),
                "@agentclientprotocol/codex-acp@1.1.2".to_string(),
            ],
        );

        assert_eq!(result, None);
    }

    #[test]
    #[should_panic(expected = "must be greater than zero")]
    fn rejects_zero_stream_idle_retention() {
        parse_positive_duration("ACPX_STREAM_IDLE_RETENTION_SECS", "0");
    }
}
