//! Method classification (gateway-native vs. proxied vs. hybrid) per
//! `02-architecture.md`'s classification table. Phase 1 only needs
//! classification for the single-agent passthrough set; profile
//! resolution, MCP-server merge, and gateway-native handlers land in
//! Phase 2/3.

use crate::keystore::Keystore;
use crate::mcp_servers::McpServerStore;
use crate::persistence::{Direction, PersistenceStore};
use crate::profile::{Profile, ProfileStore};
use crate::provider::ProviderStore;
use crate::session_registry::{BackendSessionId, SessionRegistry};

/// Which bucket a given JSON-RPC method falls into. See the classification
/// table in `02-architecture.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodClass {
    /// Handled entirely in-process; no backend agent involved.
    GatewayNative,
    /// Session-resolve + forward, payload untouched.
    Proxied,
    /// One-time gateway logic (profile/agent resolution + spawn), then
    /// delegates to the backend.
    Hybrid,
    /// Not a recognized ACP or acpx method.
    Unknown,
}

/// Classify a JSON-RPC method name. Pure function, no state -- routing
/// state (session registry, profile store, conductor) lives in `Router`.
pub fn classify(method: &str) -> MethodClass {
    match method {
        "session/new" => MethodClass::Hybrid,
        "session/prompt" | "session/resume" | "session/load" | "session/close"
        | "session/set_mode" | "session/cancel"
        // `session/set_config_option`: not one of the plan's originally
        // enumerated ACP methods, but a real, published extension method
        // used by `@agentclientprotocol/claude-agent-acp` (and, per the
        // ACP ecosystem's `configOptions` pattern surfaced on every
        // `session/new` response, likely other adapters too) for
        // in-session model selection -- verified against the real
        // published adapter, see `acpx/COVERAGE.md`'s "real multi-agent
        // concurrency" section for how this was discovered. Session-scoped
        // (carries `sessionId`, forwarded byte-for-byte like every other
        // proxied method) so it fits this bucket exactly; omitting it
        // meant a real client had no way to ever pick a non-default model
        // for a claude-agent-acp-backed profile through the gateway.
        | "session/set_config_option" => MethodClass::Proxied,
        "agents/list" | "agents/install" | "agents/status" | "session/list" => {
            MethodClass::GatewayNative
        }
        "profiles/create" | "profiles/list" | "profiles/update" | "profiles/delete" => {
            MethodClass::GatewayNative
        }
        "mcp_servers/create" | "mcp_servers/list" | "mcp_servers/update" | "mcp_servers/delete" => {
            MethodClass::GatewayNative
        }
        _ => MethodClass::Unknown,
    }
}

/// Phase 1 stub: the real `Router` composes `SessionRegistry` +
/// `acpx-conductor::Supervisor` + (from Phase 3) `ProfileStore` to actually
/// dispatch. Left unimplemented here; `acpx-server`'s Phase 1 spike talks to
/// `acpx-conductor` directly for its single hardcoded backend instead of
/// going through this type, per `04-phased-plan.md` step 4's "validates the
/// framing/spawn/proxy plumbing in isolation before adding gateway
/// complexity" note.
///
/// Phase 2 step 9: this is now the real multi-agent router. It owns an
/// `acpx_conductor::Supervisor` (spawn/reuse backend processes) and a
/// `SessionRegistry` (gateway <-> backend session id mapping), and
/// dispatches each JSON-RPC request per the classification table above:
/// gateway-native methods are answered in-process, proxied methods are
/// session-resolved and forwarded byte-for-byte (only the `sessionId`
/// field is rewritten in place), and `session/new` is hybrid (resolve
/// agent -> ensure process running -> forward -> register the returned
/// backend session id under a fresh gateway session id).
///
/// **Formerly a "Known Phase 2 gap", now closed** (was tracked in
/// `05-open-risks.md`'s "Reverse-direction (agent-initiated) messages"
/// item): agent-initiated messages that arrive on a backend's stdout
/// without a matching request id (overwhelmingly `session/update`
/// notifications -- confirmed against a real published adapter, not just
/// a hypothetical) are no longer dropped. `read_matching_response`
/// collects them and every proxied/hybrid dispatch path folds them into
/// the JSON-RPC response envelope's `_acpx.updates` array -- see
/// `read_matching_response`'s doc comment for the full rationale and
/// `acpx/COVERAGE.md`'s "real ACP content delivery" section for why this
/// mattered: without it, a client talking to a real streaming-style
/// adapter through acpx got a `session/prompt` result with no actual
/// reply text in it, ever.
pub struct Router {
    supervisor: acpx_conductor::Supervisor,
    sessions: SessionRegistry,
    /// Fallback agent id used when `session/new` carries no `_acpx.profile`
    /// (native/unmanaged mode, see `02-architecture.md`) -- Phase 3 adds
    /// real profile -> agent resolution; until then every session goes to
    /// this one configured agent.
    default_agent_id: String,
    /// HTTP client for `acpx-registry`'s live registry fetch. Reused across
    /// calls rather than constructed per-request.
    http: reqwest::Client,
    /// Lazily-populated cache of the last successful registry fetch (live
    /// or fallback) -- `agents/list`/`agents/status`/`agents/install` all
    /// refresh it via `ensure_registry_loaded` rather than re-fetching on
    /// every call. No TTL/invalidation yet; a later phase can add one if
    /// the registry needs to be re-polled for changes mid-run.
    registry_cache: Option<acpx_registry::Registry>,
    /// Optional sqlite-backed persistence (Phase 2 step 10, see
    /// `crate::persistence`). `None` by default -- a `Router` used purely
    /// in-memory (e.g. most of this crate's own tests) never touches
    /// sqlite at all. When set via [`Router::with_persistence`], session
    /// metadata and transcripts are written fire-and-forget via
    /// `tokio::spawn` off the dispatch hot path, per that module's
    /// "written asynchronously" design goal -- a slow/failed persistence
    /// write never delays or fails the client's actual request.
    persistence: Option<PersistenceStore>,
    /// Phase 3 stores: provider config, secret material, and
    /// {agent, provider, key-ref, launch overrides, mcp servers} profiles.
    /// All in-memory only (see `crate::provider`/`crate::profile`'s doc
    /// comments for why -- not persisted to the sqlite `persistence` path
    /// used for sessions/transcripts). `session/new`'s `_acpx.profile`
    /// resolves against `profiles`, which in turn references `providers`
    /// and `keystore`.
    providers: ProviderStore,
    keystore: Keystore,
    profiles: ProfileStore,
    /// Centrally-registered MCP servers (Phase 3 step 17a), merged by
    /// name into a resolved profile's `mcpServers` at `session/new` --
    /// client entries always win on collision, see
    /// `crate::mcp_servers::merge_mcp_servers`.
    mcp_servers: McpServerStore,
}

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("request has no \"method\" field")]
    MissingMethod,
    #[error("request has no \"params\" field")]
    MissingParams,
    #[error("request has no \"id\" field")]
    MissingId,
    #[error("unknown method: {0}")]
    UnknownMethod(String),
    #[error("params.sessionId missing or not a string")]
    MissingSessionId,
    #[error("no session registered for gateway session id {0}")]
    UnknownSession(String),
    #[error("backend response to session/new has no result.sessionId")]
    MissingBackendSessionId,
    #[error("backend rejected session/new: {0}")]
    BackendSessionNewError(serde_json::Value),
    #[error("{0} is not implemented yet")]
    NotImplemented(&'static str),
    #[error(transparent)]
    Supervisor(#[from] acpx_conductor::supervisor::SupervisorError),
    #[error(transparent)]
    Framing(#[from] acpx_conductor::framing::FramingError),
    #[error("agents/status: unknown agent id {0}")]
    UnknownAgentId(String),
    #[error(transparent)]
    Install(#[from] acpx_registry::InstallError),
    #[error("agents/install: missing or non-string params.id")]
    MissingAgentId,
    #[error("profile store: {0}")]
    Profile(#[from] crate::profile::ProfileStoreError),
    #[error("provider store: {0}")]
    Provider(#[from] crate::provider::ProviderStoreError),
    #[error("mcp server store: {0}")]
    McpServer(#[from] crate::mcp_servers::McpServerStoreError),
    #[error("keystore: {0}")]
    Keystore(#[from] crate::keystore::KeystoreError),
    #[error("session/new: no profile named {0}")]
    UnknownProfile(String),
    #[error("profile {profile} references unknown provider {provider}")]
    UnknownProviderRef { profile: String, provider: String },
    #[error("profile {profile}'s agent id {agent_id} has no npx/uvx distribution in the registry")]
    NoLaunchableDistribution { profile: String, agent_id: String },
}

impl Router {
    pub fn new(default_agent_id: impl Into<String>) -> Self {
        Self {
            supervisor: acpx_conductor::Supervisor::new(),
            sessions: SessionRegistry::new(),
            default_agent_id: default_agent_id.into(),
            http: reqwest::Client::new(),
            registry_cache: None,
            persistence: None,
            providers: ProviderStore::new(),
            keystore: Keystore::new(),
            profiles: ProfileStore::new(),
            mcp_servers: McpServerStore::new(),
        }
    }

    /// Attach a [`PersistenceStore`] -- session metadata and transcripts
    /// are recorded from that point on. Builder-style so callers can write
    /// `Router::new(id).with_persistence(store)`.
    pub fn with_persistence(mut self, store: PersistenceStore) -> Self {
        self.persistence = Some(store);
        self
    }

    /// Register how to spawn a given agent id. Mirrors
    /// `Supervisor::register` -- `Router` doesn't reinterpret the spec, it
    /// just owns the `Supervisor` instance.
    pub fn register_agent(&mut self, agent_id: impl Into<String>, spec: acpx_conductor::SpawnSpec) {
        self.supervisor.register(agent_id, spec);
    }

    /// Seed a provider config, overwriting any existing entry of the same
    /// name. Server-side-only seam -- there is deliberately no
    /// `providers/*` JSON-RPC method a remote client can call (per the
    /// task draft's "keys are maintained by this intermediate proxy": a
    /// provider's `base_url` plus whatever key a profile references are
    /// gateway-provisioned configuration, not something a client should
    /// ever be able to set for itself). `acpx-server`'s `main.rs` is the
    /// intended caller, loading providers from its own startup config;
    /// tests use it directly too.
    pub fn register_provider(&mut self, provider: crate::provider::ProviderConfig) {
        let name = provider.name.clone();
        if self.providers.update(provider.clone()).is_err() {
            let _ = self.providers.create(provider);
            let _ = name; // update() already logged nothing; create() covers the fresh-entry case
        }
    }

    /// Store a raw secret, returning its opaque [`crate::keystore::KeyRef`]
    /// for a [`crate::profile::Profile::key_ref`]. Same server-side-only
    /// rationale as [`Self::register_provider`] -- see that method's doc
    /// comment.
    pub fn store_key(&mut self, secret: impl Into<String>) -> crate::keystore::KeyRef {
        self.keystore.store(secret)
    }

    /// Test/observability seam: live-process status for a given
    /// supervisor key (a native mode agent id, or a profile's
    /// `"profile:<name>"` key -- see `resolve_profile`). Distinct from the
    /// `agents/status` JSON-RPC method, which answers a different
    /// question (whether the runtime/binary needed to launch an agent is
    /// present at all, via `crate::detect`), not "is a process currently
    /// running under this exact supervisor key right now".
    pub fn process_status(
        &mut self,
        supervisor_key: &str,
    ) -> acpx_conductor::supervisor::ProcessStatus {
        self.supervisor.status(supervisor_key)
    }

    /// Fire-and-forget one transcript append, if persistence is attached.
    /// Never awaited by the caller -- spawned onto the runtime so a slow
    /// sqlite write can't add latency to the client-visible request path.
    fn spawn_transcript(
        &self,
        gateway_session_id: impl Into<String>,
        direction: Direction,
        payload: serde_json::Value,
    ) {
        spawn_transcript_fn(
            self.persistence.clone(),
            gateway_session_id,
            direction,
            payload,
        );
    }

    /// Fire-and-forget persistence for a freshly-registered session:
    /// `record_session` followed by the two `session/new` transcript rows
    /// (client request, agent response), all inside a *single* spawned
    /// task so the writes are strictly ordered.
    ///
    /// This matters beyond bookkeeping: `transcripts.gateway_session_id`
    /// has a `FOREIGN KEY` on `sessions.gateway_session_id` (see
    /// `persistence/mod.rs`'s `SCHEMA_SQL`). Spawning `record_session` and
    /// the transcript appends as three *independent* `tokio::spawn` tasks
    /// (as this used to do) races them against each other -- if either
    /// transcript insert's blocking-pool task got scheduled before
    /// `record_session`'s, sqlite rejected it with `FOREIGN KEY constraint
    /// failed`, and because these are fire-and-forget the error was only
    /// ever logged via `tracing::warn!`, never surfacing to a caller. That
    /// produced the exact flake seen in `router_persistence_test.rs`:
    /// `list_transcripts` intermittently stuck at 0 or 1 instead of 2.
    /// Doing all three writes sequentially inside one task preserves the
    /// "never block the client-visible request path" property while
    /// guaranteeing the parent `sessions` row always lands first.
    fn spawn_session_persistence(
        &self,
        gateway_session_id: impl Into<String>,
        agent_id: impl Into<String>,
        backend_session_id: impl Into<String>,
        profile_name: Option<String>,
        client_request: serde_json::Value,
        agent_response: serde_json::Value,
    ) {
        spawn_session_persistence_fn(
            self.persistence.clone(),
            gateway_session_id,
            agent_id,
            backend_session_id,
            profile_name,
            client_request,
            agent_response,
        );
    }

    /// Ensure the registry cache is populated, fetching (live, falling
    /// back to the bundled snapshot on any error) if it hasn't been yet.
    /// See `acpx_registry::fetch_registry_or_fallback` -- this never
    /// itself fails, matching that function's contract.
    async fn ensure_registry_loaded(&mut self) -> &acpx_registry::Registry {
        if self.registry_cache.is_none() {
            self.registry_cache = Some(acpx_registry::fetch_registry_or_fallback(&self.http).await);
        }
        self.registry_cache.as_ref().expect("just populated")
    }

    /// Dispatch one JSON-RPC request, returning the JSON-RPC response to
    /// send back to the client that issued it.
    pub async fn dispatch(
        &mut self,
        request: serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        let method = request
            .get("method")
            .and_then(|m| m.as_str())
            .ok_or(RouterError::MissingMethod)?
            .to_string();
        match classify(&method) {
            MethodClass::Hybrid => self.dispatch_session_new(request).await,
            MethodClass::Proxied => self.dispatch_proxied(request).await,
            MethodClass::GatewayNative => self.dispatch_native(&method, request).await,
            MethodClass::Unknown => Err(RouterError::UnknownMethod(method)),
        }
    }

    async fn dispatch_session_new(
        &mut self,
        mut request: serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;
        let params = request
            .get_mut("params")
            .ok_or(RouterError::MissingParams)?;

        // Precedence per 02-architecture.md: an explicit `_acpx.profile`
        // selects managed mode (Phase 3: profile -> agent/provider/key
        // resolution, see `resolve_profile`); omitting it stays
        // native/unmanaged, using `default_agent_id`'s already-registered
        // spawn spec unchanged. Either way `_acpx` is stripped before
        // forwarding -- session/new stays a raw-ACP drop-in for a client
        // that never set it.
        let profile_name = params
            .get("_acpx")
            .and_then(|ext| ext.get("profile"))
            .and_then(|p| p.as_str())
            .map(str::to_string);
        if let Some(obj) = params.as_object_mut() {
            obj.remove("_acpx");
        }

        let (agent_id, profile) = match &profile_name {
            Some(name) => {
                let (supervisor_key, profile) = self.resolve_profile(name).await?;
                (supervisor_key, Some(profile))
            }
            None => (self.default_agent_id.clone(), None),
        };

        // Merge the resolved profile's centrally-registered MCP servers
        // into whatever the client itself sent, client entries winning on
        // name collision -- see `crate::mcp_servers::merge_mcp_servers`.
        // A no-op (params untouched) when the profile has no attached
        // servers, so native mode and profiles with `mcp_servers: []`
        // never see an `mcpServers` field appear out of nowhere.
        if let Some(profile) = &profile {
            if !profile.mcp_servers.is_empty() {
                let central = self.mcp_servers.list_named(&profile.mcp_servers);
                let params = request
                    .get_mut("params")
                    .ok_or(RouterError::MissingParams)?;
                let client_servers = params
                    .get("mcpServers")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let merged = crate::mcp_servers::merge_mcp_servers(&client_servers, &central);
                if let Some(obj) = params.as_object_mut() {
                    obj.insert("mcpServers".to_string(), serde_json::json!(merged));
                }
            }
        }

        let backend = self.supervisor.ensure_running(&agent_id).await?;
        let mut response = {
            let mut backend = backend.lock().await;
            ensure_backend_initialized(&mut backend).await?;
            backend.writer.write_value(&request).await?;
            let (response, notifications) = read_matching_response(&mut backend, &id).await?;
            attach_updates(response, notifications)
        };

        let backend_session_id = extract_backend_session_id(&response)?;
        let gateway_id = self
            .sessions
            .register(agent_id, BackendSessionId(backend_session_id));

        // Rewrite the backend's own session id into the gateway-issued one
        // before it ever reaches the client -- the client only ever sees
        // gateway session ids, never a raw backend id.
        let gateway_session_id_str = gateway_id.0.clone();
        if let Some(result) = response.get_mut("result") {
            result["sessionId"] = serde_json::Value::String(gateway_id.0);
        }
        if let Some(entry) = self
            .sessions
            .resolve(&acpx_proto::session::GatewaySessionId(
                gateway_session_id_str.clone(),
            ))
        {
            self.spawn_session_persistence(
                gateway_session_id_str,
                entry.agent_id.clone(),
                entry.backend_session_id.0.clone(),
                profile.map(|p| p.name),
                request,
                response.clone(),
            );
        }
        Ok(response)
    }

    /// Resolve `_acpx.profile` for `session/new`'s managed mode: look up
    /// the named `Profile`, resolve its `provider`/`key_ref` (if any) into
    /// concrete env vars via `crate::launch::build_launch_env`, and
    /// (re-)register a `SpawnSpec` for it under a per-profile supervisor
    /// key (`"profile:<name>"`, distinct from any native-mode agent id so
    /// the two never share -- or fight over -- one supervised process).
    /// Returns `(supervisor_key, profile)` for the caller to spawn/proxy
    /// against and to read `mcp_servers`/`name` off of.
    ///
    /// **Known Phase 3 gap** (see `05-open-risks.md`'s "one process per
    /// backend vs. one process per session" item): re-resolving an
    /// already-running profile's env here does *not* restart its
    /// supervised process -- `Supervisor::ensure_running` only spawns a
    /// fresh process when none is currently running under this key, so a
    /// `profiles/update` that changes a profile's provider/key only takes
    /// effect for the *next* profile that has no live process yet, not for
    /// an already-running one. Not fixed here; flagged, not silently
    /// wrong.
    async fn resolve_profile(
        &mut self,
        profile_name: &str,
    ) -> Result<(String, Profile), RouterError> {
        let profile = self
            .profiles
            .get(profile_name)
            .cloned()
            .ok_or_else(|| RouterError::UnknownProfile(profile_name.to_string()))?;

        let provider = match &profile.provider {
            Some(name) => Some(self.providers.get(name).cloned().ok_or_else(|| {
                RouterError::UnknownProviderRef {
                    profile: profile.name.clone(),
                    provider: name.clone(),
                }
            })?),
            None => None,
        };
        let resolved_key = match &profile.key_ref {
            Some(key_ref) => Some(self.keystore.resolve(key_ref)?.to_string()),
            None => None,
        };
        let env =
            crate::launch::build_launch_env(&profile, provider.as_ref(), resolved_key.as_deref());

        // Prefer an already-registered `SpawnSpec` for `profile.agent_id`
        // (e.g. the native default agent, or anything an operator/test
        // registered directly via `Router::register_agent`) as the base
        // to layer env onto -- only fall back to a fresh registry lookup
        // (building an `npx`/`uvx` `SpawnSpec` from scratch) when nothing
        // is registered under that id yet. This keeps profiles usable
        // against both registry-listed agents (the common case) and
        // manually-configured/non-registry backends, without forcing a
        // registry fetch on every `session/new` for the latter.
        let mut spec = match self.supervisor.spec(&profile.agent_id).cloned() {
            Some(spec) => spec,
            None => {
                self.ensure_registry_loaded().await;
                let agent = self
                    .registry_cache
                    .as_ref()
                    .expect("just loaded")
                    .agents
                    .iter()
                    .find(|a| a.id == profile.agent_id)
                    .cloned()
                    .ok_or_else(|| RouterError::UnknownAgentId(profile.agent_id.clone()))?;
                npx_spawn_spec(&agent).ok_or_else(|| RouterError::NoLaunchableDistribution {
                    profile: profile.name.clone(),
                    agent_id: profile.agent_id.clone(),
                })?
            }
        };
        // Overlay (not replace) so a manually-registered base spec's own
        // env (if any) survives for any var the profile doesn't itself
        // derive/override.
        for (key, value) in env {
            spec.env.insert(key, value);
        }

        let supervisor_key = format!("profile:{}", profile.name);
        self.supervisor.register(supervisor_key.clone(), spec);
        Ok((supervisor_key, profile))
    }

    async fn dispatch_proxied(
        &mut self,
        mut request: serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        let method = request
            .get("method")
            .and_then(|m| m.as_str())
            .ok_or(RouterError::MissingMethod)?
            .to_string();
        let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;
        let params = request
            .get_mut("params")
            .ok_or(RouterError::MissingParams)?;
        let gateway_session_id = params
            .get("sessionId")
            .and_then(|s| s.as_str())
            .ok_or(RouterError::MissingSessionId)?
            .to_string();

        let entry = self
            .sessions
            .resolve(&acpx_proto::session::GatewaySessionId(
                gateway_session_id.clone(),
            ))
            .ok_or_else(|| RouterError::UnknownSession(gateway_session_id.clone()))?;
        let agent_id = entry.agent_id.clone();
        let backend_session_id = entry.backend_session_id.0.clone();

        // Rewrite gateway id -> backend id in place; everything else in
        // `params` is forwarded untouched, per the proxied-method contract
        // in 02-architecture.md.
        params["sessionId"] = serde_json::Value::String(backend_session_id);

        self.spawn_transcript(
            gateway_session_id.clone(),
            Direction::ClientToAgent,
            request.clone(),
        );

        let backend = self.supervisor.ensure_running(&agent_id).await?;
        let response = {
            let mut backend = backend.lock().await;
            ensure_backend_initialized(&mut backend).await?;
            backend.writer.write_value(&request).await?;
            let (response, notifications) = read_matching_response(&mut backend, &id).await?;
            attach_updates(response, notifications)
        };
        self.spawn_transcript(
            gateway_session_id.clone(),
            Direction::AgentToClient,
            response.clone(),
        );
        if method == "session/close" {
            // Evict the closed session from the in-memory registry --
            // **real bug fix**: this used to never happen, so every
            // session ever opened over a long-running daemon's lifetime
            // stayed in `SessionRegistry`'s `HashMap` forever (an
            // unbounded memory leak) and `session/list` kept reporting
            // closed sessions as still live indefinitely. `remove` already
            // existed on `SessionRegistry` but was never called from
            // anywhere in this file until now.
            self.sessions.remove(&acpx_proto::session::GatewaySessionId(
                gateway_session_id.clone(),
            ));
            if let Some(store) = self.persistence.clone() {
                tokio::spawn(async move {
                    if let Err(err) = store.close_session(gateway_session_id, now_rfc3339()).await {
                        tracing::warn!(%err, "failed to persist session close");
                    }
                });
            }
        }
        Ok(response)
    }

    async fn dispatch_native(
        &mut self,
        method: &str,
        request: serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;
        let result = match method {
            "session/list" => {
                let sessions: Vec<serde_json::Value> = self
                    .sessions
                    .list()
                    .map(|(gateway_id, entry)| {
                        serde_json::json!({
                            "sessionId": gateway_id,
                            "agentId": entry.agent_id,
                        })
                    })
                    .collect();
                serde_json::json!({ "sessions": sessions })
            }
            "agents/list" => {
                self.ensure_registry_loaded().await;
                let agents = self
                    .registry_cache
                    .as_ref()
                    .expect("just loaded")
                    .agents
                    .clone();
                let entries: Vec<serde_json::Value> = agents
                    .into_iter()
                    .map(|agent| {
                        let status = crate::detect::detect(&agent.id, &agent.distribution);
                        serde_json::json!({
                            "id": agent.id,
                            "name": agent.name,
                            "version": agent.version,
                            "status": status,
                        })
                    })
                    .collect();
                serde_json::json!({ "agents": entries })
            }
            "agents/status" => {
                let agent_id = request
                    .get("params")
                    .and_then(|p| p.get("id"))
                    .and_then(|i| i.as_str())
                    .ok_or(RouterError::MissingParams)?
                    .to_string();
                self.ensure_registry_loaded().await;
                let agent = self
                    .registry_cache
                    .as_ref()
                    .expect("just loaded")
                    .agents
                    .iter()
                    .find(|a| a.id == agent_id)
                    .cloned()
                    .ok_or(RouterError::UnknownAgentId(agent_id))?;
                let status = crate::detect::detect(&agent.id, &agent.distribution);
                serde_json::json!({ "id": agent.id, "status": status })
            }
            "agents/install" => {
                let agent_id = request
                    .get("params")
                    .and_then(|p| p.get("id"))
                    .and_then(|i| i.as_str())
                    .ok_or(RouterError::MissingAgentId)?
                    .to_string();
                self.ensure_registry_loaded().await;
                let agent = self
                    .registry_cache
                    .as_ref()
                    .expect("just loaded")
                    .agents
                    .iter()
                    .find(|a| a.id == agent_id)
                    .cloned()
                    .ok_or(RouterError::UnknownAgentId(agent_id))?;
                let outcome = acpx_registry::install(&agent).await?;
                serde_json::json!({ "id": agent.id, "outcome": format!("{outcome:?}") })
            }
            "profiles/create" | "profiles/update" => {
                let params = request
                    .get("params")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let mut profile: Profile = serde_json::from_value(params.clone())
                    .map_err(|_| RouterError::MissingParams)?;
                // A raw `secret` field (never itself echoed back, see below)
                // is stored via `Keystore::store` and the resulting
                // opaque `KeyRef` wins over any `key_ref` the caller sent
                // directly -- `profiles/create`/`update` is the only entry
                // point for getting a secret into the keystore at all
                // (Phase 3 scoped no separate `keys/*` JSON-RPC namespace,
                // see `04-phased-plan.md` step 13/14).
                if let Some(secret) = params.get("secret").and_then(|s| s.as_str()) {
                    profile.key_ref = Some(self.keystore.store(secret));
                }
                if method == "profiles/create" {
                    self.profiles.create(profile.clone())?;
                } else {
                    self.profiles.update(profile.clone())?;
                }
                redact_launch_overrides(
                    serde_json::to_value(&profile).expect("Profile always serializes"),
                )
            }
            "profiles/list" => {
                let profiles: Vec<serde_json::Value> = self
                    .profiles
                    .list()
                    .map(|p| {
                        redact_launch_overrides(
                            serde_json::to_value(p).expect("Profile always serializes"),
                        )
                    })
                    .collect();
                serde_json::json!({ "profiles": profiles })
            }
            "profiles/delete" => {
                let name = request
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .ok_or(RouterError::MissingParams)?
                    .to_string();
                self.profiles.delete(&name)?;
                // **Real bug fix**: this used to only remove the
                // `ProfileStore` entry, leaving whatever backend process
                // was spawned for it (under supervisor key
                // `"profile:<name>"`, see `resolve_profile`) running
                // forever with no way to ever stop it again -- an orphaned
                // OS child process leaked on every `profiles/delete` call
                // against a profile that had ever actually been used in a
                // `session/new`. Best-effort: `Supervisor::stop` is a
                // no-op (not an error) if no process was ever spawned
                // under this key, so this is safe to call unconditionally
                // regardless of whether the profile was ever used.
                let supervisor_key = format!("profile:{name}");
                if let Err(err) = self.supervisor.stop(&supervisor_key).await {
                    tracing::warn!(%err, profile = %name, "failed to stop profile's backend process on delete");
                }
                serde_json::json!({ "name": name, "deleted": true })
            }
            "mcp_servers/create" | "mcp_servers/update" => {
                let entry = request
                    .get("params")
                    .cloned()
                    .ok_or(RouterError::MissingParams)?;
                if method == "mcp_servers/create" {
                    self.mcp_servers.create(entry.clone())?;
                } else {
                    self.mcp_servers.update(entry.clone())?;
                }
                entry
            }
            "mcp_servers/list" => {
                serde_json::json!({ "servers": self.mcp_servers.list() })
            }
            "mcp_servers/delete" => {
                let name = request
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .ok_or(RouterError::MissingParams)?
                    .to_string();
                self.mcp_servers.delete(&name)?;
                serde_json::json!({ "name": name, "deleted": true })
            }
            other => return Err(RouterError::UnknownMethod(other.to_string())),
        };
        Ok(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }))
    }
}

/// Read backend messages until one whose `id` matches the request we just
/// sent, collecting every unmatched message seen along the way (almost
/// always `session/update` notifications -- an agent-initiated request
/// with no matching id is vanishingly rare and, if it ever happens, gets
/// collected the same way rather than silently dropped either).
///
/// **Reverse-direction routing (closes the former "Known Phase 2 gap",
/// see `acpx/COVERAGE.md`'s "real ACP content delivery" section for the
/// full story of why this mattered in practice):** real ACP agents (every
/// adapter checked against a real published npx package, not just the
/// synthetic stand-ins used elsewhere in this workspace's tests) deliver
/// the actual assistant reply text via `session/update`
/// `agent_message_chunk` notifications streamed *during* a `session/
/// prompt` call -- the call's own JSON-RPC result is just `{stopReason,
/// usage}`, with no message content at all. Silently dropping every
/// notification (this function's pre-fix behavior) meant a client talking
/// to a real adapter through acpx got back a result with no actual answer
/// in it, ever -- a correctness bug serious enough that it made "acpx
/// client working end to end against a real backend" false regardless of
/// anything else in this gateway working correctly.
///
/// The fix returns every collected notification alongside the matched
/// response; every caller below folds them into the JSON-RPC envelope's
/// `_acpx.updates` field (additive, namespaced, ignorable by any raw ACP
/// client that doesn't know about it) rather than a true live push --
/// see `dispatch_proxied`/`dispatch_proxied_shared`'s doc comments for why
/// aggregation-into-the-response is the right fit for this gateway's
/// request/response-shaped transports (HTTP chief among them) rather than
/// a separate out-of-band push channel.
/// Fixed request id for the one-time ACP `initialize` handshake performed
/// against a backend process the first time it's used (see
/// [`ensure_backend_initialized`]'s doc comment). Numeric (not a string
/// id) deliberately -- this workspace's synthetic `sh -c '...'` stand-in
/// backends (used by roughly a dozen pre-existing tests) extract the
/// request id with a numeric-only shell regex (`grep -o '"id":[0-9]*'`)
/// and echo it back verbatim; a string id would make every one of those
/// scripts emit malformed JSON (`"id":`) in reply. Never collides with a
/// real client's own request id in practice: this handshake always
/// completes (or fails the whole dispatch) before the actual client
/// request is ever written to the same backend.
const INITIALIZE_REQUEST_ID: i64 = 0;

/// Perform the real ACP `initialize` request/response round trip against
/// `proc` if it hasn't already happened for this exact process instance
/// (`BackendProcess::handshake_done`, reset to `false` on every fresh
/// spawn -- see that field's doc comment).
///
/// **Real bug this fixes** (found driving `real_claude_multi_agent_test.rs`
/// against a real, published `@agentclientprotocol/claude-agent-acp`
/// adapter, not a synthetic stand-in): every dispatch path wrote
/// `session/new` as the very first message on a freshly spawned backend's
/// stdio, with no ACP `initialize` handshake ever performed first. This
/// workspace's ~120 pre-existing tests never caught it because their
/// synthetic `sh -c '...'` stand-in backends answer *any* request
/// uniformly regardless of ordering. A real adapter does not: verified
/// against claude-agent-acp, it silently omits `result.sessionId` from
/// its `session/new` response if `initialize` was never called first,
/// which acpx surfaced as an opaque `RouterError::MissingBackendSessionId`
/// with no indication the real problem was protocol ordering, not the
/// request itself.
async fn ensure_backend_initialized(
    proc: &mut acpx_conductor::BackendProcess,
) -> Result<(), RouterError> {
    if proc.handshake_done {
        return Ok(());
    }
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": INITIALIZE_REQUEST_ID,
        "method": "initialize",
        "params": {
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": { "readTextFile": false, "writeTextFile": false }
            }
        }
    });
    proc.writer.write_value(&request).await?;
    loop {
        let value = proc.reader.read_value().await?;
        if value.get("id").and_then(|v| v.as_i64()) == Some(INITIALIZE_REQUEST_ID) {
            break;
        }
        // A well-behaved adapter shouldn't emit anything unprompted
        // before answering `initialize`, but stay defensive rather than
        // assuming the very first line back is necessarily the match --
        // `read_value`'s own `FramingError::Eof` on a closed pipe is
        // still the hard stop if the backend never answers at all.
    }
    proc.handshake_done = true;
    Ok(())
}

/// Pulls `result.sessionId` out of a `session/new` response, or a proper
/// [`RouterError::BackendSessionNewError`] carrying the backend's own
/// JSON-RPC `error` object if it sent one -- **discovered as a real
/// debugging pain point** driving `real_claude_multi_agent_test.rs`: the
/// old code only ever checked for a missing `result.sessionId`, so a
/// backend that legitimately rejected the request (e.g. claude-agent-acp
/// returning a real `-32602 Invalid params` for a `session/new` missing
/// its required `mcpServers` field) surfaced through acpx as an opaque
/// "no result.sessionId" with the actual rejection reason silently
/// dropped, not forwarded.
fn extract_backend_session_id(response: &serde_json::Value) -> Result<String, RouterError> {
    if let Some(error) = response.get("error") {
        return Err(RouterError::BackendSessionNewError(error.clone()));
    }
    response
        .get("result")
        .and_then(|r| r.get("sessionId"))
        .and_then(|s| s.as_str())
        .map(str::to_string)
        .ok_or(RouterError::MissingBackendSessionId)
}

async fn read_matching_response(
    backend: &mut acpx_conductor::BackendProcess,
    id: &serde_json::Value,
) -> Result<(serde_json::Value, Vec<serde_json::Value>), RouterError> {
    let mut notifications = Vec::new();
    loop {
        let value = backend.reader.read_value().await?;
        if value.get("id") == Some(id) {
            return Ok((value, notifications));
        }
        notifications.push(value);
    }
}

/// Fold `notifications` (as collected by [`read_matching_response`]) into
/// `response`'s `_acpx.updates` array, if there are any. No-op (response
/// left byte-for-byte untouched) when `notifications` is empty, so a
/// stand-in backend that never emits `session/update` at all (every
/// synthetic test double in this workspace) produces identical response
/// shapes to before this fix -- verified by every pre-existing test in
/// this workspace continuing to pass unmodified.
fn attach_updates(
    mut response: serde_json::Value,
    notifications: Vec<serde_json::Value>,
) -> serde_json::Value {
    if notifications.is_empty() {
        return response;
    }
    if let Some(obj) = response.as_object_mut() {
        obj.insert(
            "_acpx".to_string(),
            serde_json::json!({ "updates": notifications }),
        );
    }
    response
}

/// Free-function twin of `Router::spawn_transcript`, taking an already-
/// cloned `Option<PersistenceStore>` instead of `&self` -- shared by both
/// the original `&mut self` dispatch path and [`dispatch_shared`]'s
/// unlock-during-backend-I/O path below, so the two never drift apart.
fn spawn_transcript_fn(
    store: Option<PersistenceStore>,
    gateway_session_id: impl Into<String>,
    direction: Direction,
    payload: serde_json::Value,
) {
    let Some(store) = store else {
        return;
    };
    let gateway_session_id = gateway_session_id.into();
    tokio::spawn(async move {
        if let Err(err) = store
            .append_transcript(gateway_session_id, direction, payload, now_rfc3339())
            .await
        {
            tracing::warn!(%err, "failed to persist transcript entry");
        }
    });
}

/// Free-function twin of `Router::spawn_session_persistence` -- see
/// `spawn_transcript_fn`'s doc comment for why this split exists.
#[allow(clippy::too_many_arguments)]
fn spawn_session_persistence_fn(
    store: Option<PersistenceStore>,
    gateway_session_id: impl Into<String>,
    agent_id: impl Into<String>,
    backend_session_id: impl Into<String>,
    profile_name: Option<String>,
    client_request: serde_json::Value,
    agent_response: serde_json::Value,
) {
    let Some(store) = store else {
        return;
    };
    let gateway_session_id = gateway_session_id.into();
    let agent_id = agent_id.into();
    let backend_session_id = backend_session_id.into();
    tokio::spawn(async move {
        if let Err(err) = store
            .record_session(
                gateway_session_id.clone(),
                agent_id,
                backend_session_id,
                profile_name,
                now_rfc3339(),
            )
            .await
        {
            tracing::warn!(%err, "failed to persist session metadata");
            return;
        }
        if let Err(err) = store
            .append_transcript(
                gateway_session_id.clone(),
                Direction::ClientToAgent,
                client_request,
                now_rfc3339(),
            )
            .await
        {
            tracing::warn!(%err, "failed to persist transcript entry");
        }
        if let Err(err) = store
            .append_transcript(
                gateway_session_id,
                Direction::AgentToClient,
                agent_response,
                now_rfc3339(),
            )
            .await
        {
            tracing::warn!(%err, "failed to persist transcript entry");
        }
    });
}

/// `Arc<tokio::sync::Mutex<Router>>` -- the handle type
/// `acpx-server`'s transports hold and pass to [`dispatch_shared`].
/// Re-exported here (rather than only living in `acpx-server`) so this
/// module can define `dispatch_shared` against it directly.
pub type SharedRouterHandle = std::sync::Arc<tokio::sync::Mutex<Router>>;

/// Real multi-agent concurrency entry point (added post-Phase-6, replacing
/// the naive "hold the whole-`Router` mutex for an entire `dispatch` call,
/// including the backend's real-LLM-latency I/O" pattern every transport
/// used through Phase 6 -- see `acpx/COVERAGE.md`'s "real multi-agent
/// concurrency" section for the full writeup of why that was a genuine
/// correctness/scalability bug, not just a style preference, for a
/// "virtual gateway daemon" whose entire purpose is fronting *multiple*
/// concurrently-used backend agents).
///
/// Same [`RouterError`] contract as [`Router::dispatch`] (in fact
/// delegates to it for [`MethodClass::GatewayNative`] and
/// [`MethodClass::Unknown`], which never touch a backend process and stay
/// cheap/local). For [`MethodClass::Hybrid`] (`session/new`) and
/// [`MethodClass::Proxied`] (`session/prompt` and friends) -- the only
/// method classes that ever talk to a backend agent process -- this
/// function locks `router` only long enough to resolve gateway state
/// (session registry, profile/provider resolution, `Supervisor::
/// ensure_running`'s bookkeeping) and obtain a
/// `acpx_conductor::SharedBackendProcess` handle, then **drops that lock**
/// before doing the actual backend stdio round trip against just that
/// handle's own per-process mutex. Two concurrent callers resolving to two
/// *different* backend agents now genuinely run their I/O in parallel;
/// two callers resolving to the *same* backend agent still correctly
/// serialize on that one process's own lock (see
/// `acpx_conductor::supervisor`'s module doc comment for why that part is
/// unavoidable, not a remaining bug).
///
/// `acpx-server`'s HTTP/WS/stdio transports all call this instead of
/// `router.lock().await.dispatch(...)`; `Router::dispatch` itself is left
/// untouched and still used directly by every in-process test in this
/// workspace that constructs a bare `Router` (no sharing, no concurrency
/// to speak of), so none of those ~100 existing tests needed to change.
pub async fn dispatch_shared(
    router: &SharedRouterHandle,
    request: serde_json::Value,
) -> Result<serde_json::Value, RouterError> {
    let method = request
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or(RouterError::MissingMethod)?
        .to_string();
    match classify(&method) {
        MethodClass::Hybrid => dispatch_session_new_shared(router, request).await,
        MethodClass::Proxied => dispatch_proxied_shared(router, request).await,
        MethodClass::GatewayNative | MethodClass::Unknown => {
            router.lock().await.dispatch(request).await
        }
    }
}

/// [`dispatch_shared`]'s `session/prompt`/`session/resume`/`session/load`/
/// `session/close`/`session/set_mode`/`session/cancel` path. Mirrors
/// `Router::dispatch_proxied` exactly (session resolution, sessionId
/// rewrite, transcript persistence, `session/close` bookkeeping) but
/// restructured to release `router`'s lock before the backend round trip.
async fn dispatch_proxied_shared(
    router: &SharedRouterHandle,
    mut request: serde_json::Value,
) -> Result<serde_json::Value, RouterError> {
    let method = request
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or(RouterError::MissingMethod)?
        .to_string();
    let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;
    let gateway_session_id = request
        .get("params")
        .and_then(|p| p.get("sessionId"))
        .and_then(|s| s.as_str())
        .ok_or(RouterError::MissingSessionId)?
        .to_string();

    let (backend, persistence) = {
        let mut r = router.lock().await;
        let entry = r
            .sessions
            .resolve(&acpx_proto::session::GatewaySessionId(
                gateway_session_id.clone(),
            ))
            .ok_or_else(|| RouterError::UnknownSession(gateway_session_id.clone()))?;
        let agent_id = entry.agent_id.clone();
        let backend_session_id = entry.backend_session_id.0.clone();
        if let Some(params) = request.get_mut("params") {
            params["sessionId"] = serde_json::Value::String(backend_session_id);
        }
        let backend = r.supervisor.ensure_running(&agent_id).await?;
        (backend, r.persistence.clone())
    };

    spawn_transcript_fn(
        persistence.clone(),
        gateway_session_id.clone(),
        Direction::ClientToAgent,
        request.clone(),
    );

    let response = {
        let mut proc = backend.lock().await;
        ensure_backend_initialized(&mut proc).await?;
        proc.writer.write_value(&request).await?;
        let (response, notifications) = read_matching_response(&mut proc, &id).await?;
        attach_updates(response, notifications)
    };

    spawn_transcript_fn(
        persistence.clone(),
        gateway_session_id.clone(),
        Direction::AgentToClient,
        response.clone(),
    );

    if method == "session/close" {
        // Same leak/correctness fix as `Router::dispatch_proxied` above --
        // see that call site's comment. Re-acquire the router lock
        // briefly (bookkeeping only, no backend I/O held) to evict the
        // closed session from the shared `SessionRegistry` too, so the
        // two dispatch paths never drift apart on this behavior.
        router
            .lock()
            .await
            .sessions
            .remove(&acpx_proto::session::GatewaySessionId(
                gateway_session_id.clone(),
            ));
        if let Some(store) = persistence {
            tokio::spawn(async move {
                if let Err(err) = store.close_session(gateway_session_id, now_rfc3339()).await {
                    tracing::warn!(%err, "failed to persist session close");
                }
            });
        }
    }
    Ok(response)
}

/// [`dispatch_shared`]'s `session/new` path. Mirrors
/// `Router::dispatch_session_new` exactly (`_acpx.profile` resolution,
/// central MCP server merge, gateway session id issuance, session
/// persistence) but restructured to release `router`'s lock before the
/// backend round trip.
async fn dispatch_session_new_shared(
    router: &SharedRouterHandle,
    mut request: serde_json::Value,
) -> Result<serde_json::Value, RouterError> {
    let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;

    let (agent_id, profile, backend, persistence) = {
        let mut r = router.lock().await;
        let params = request
            .get_mut("params")
            .ok_or(RouterError::MissingParams)?;
        let profile_name = params
            .get("_acpx")
            .and_then(|ext| ext.get("profile"))
            .and_then(|p| p.as_str())
            .map(str::to_string);
        if let Some(obj) = params.as_object_mut() {
            obj.remove("_acpx");
        }

        let (agent_id, profile) = match &profile_name {
            Some(name) => {
                let (supervisor_key, profile) = r.resolve_profile(name).await?;
                (supervisor_key, Some(profile))
            }
            None => (r.default_agent_id.clone(), None),
        };

        if let Some(profile) = &profile {
            if !profile.mcp_servers.is_empty() {
                let central = r.mcp_servers.list_named(&profile.mcp_servers);
                let params = request
                    .get_mut("params")
                    .ok_or(RouterError::MissingParams)?;
                let client_servers = params
                    .get("mcpServers")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let merged = crate::mcp_servers::merge_mcp_servers(&client_servers, &central);
                if let Some(obj) = params.as_object_mut() {
                    obj.insert("mcpServers".to_string(), serde_json::json!(merged));
                }
            }
        }

        let backend = r.supervisor.ensure_running(&agent_id).await?;
        (agent_id, profile, backend, r.persistence.clone())
    };

    let mut response = {
        let mut proc = backend.lock().await;
        ensure_backend_initialized(&mut proc).await?;
        proc.writer.write_value(&request).await?;
        let (response, notifications) = read_matching_response(&mut proc, &id).await?;
        attach_updates(response, notifications)
    };

    let backend_session_id = extract_backend_session_id(&response)?;

    let (gateway_session_id_str, persist_args) = {
        let mut r = router.lock().await;
        let gateway_id = r
            .sessions
            .register(agent_id, BackendSessionId(backend_session_id));
        let gateway_session_id_str = gateway_id.0.clone();
        if let Some(result) = response.get_mut("result") {
            result["sessionId"] = serde_json::Value::String(gateway_id.0);
        }
        // Re-resolve (mirrors `Router::dispatch_session_new`'s own
        // approach) rather than threading `agent_id`/`backend_session_id`
        // back out through the closure -- `agent_id` was just moved into
        // `register` above, and this is the same lock acquisition anyway.
        let persist_args = r
            .sessions
            .resolve(&acpx_proto::session::GatewaySessionId(
                gateway_session_id_str.clone(),
            ))
            .map(|entry| (entry.agent_id.clone(), entry.backend_session_id.0.clone()));
        (gateway_session_id_str, persist_args)
    };

    if let Some((persisted_agent_id, persisted_backend_session_id)) = persist_args {
        spawn_session_persistence_fn(
            persistence,
            gateway_session_id_str,
            persisted_agent_id,
            persisted_backend_session_id,
            profile.map(|p| p.name),
            request,
            response.clone(),
        );
    }

    Ok(response)
}

/// Wall-clock timestamp for persistence rows, RFC 3339 via `SystemTime` (no
/// extra date/time crate dependency -- acpx-core doesn't otherwise need
/// one, and this precision is more than enough for session/transcript
/// bookkeeping).
fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:09}Z", now.as_secs(), now.subsec_nanos())
}

/// Mask every value in a serialized `Profile`'s `launch_overrides` map
/// before it's ever echoed back to a client (`profiles/create`/`update`'s
/// own response, and every entry in `profiles/list`).
///
/// **Real bug this closes** (found in the same self-review pass as the
/// `session/close` leak and `profiles/delete` process leak above):
/// `launch_overrides` is documented (`profile.rs`, `resolve_profile`'s
/// doc comment) as a raw env-var escape hatch specifically meant to carry
/// things like `ANTHROPIC_API_KEY`/`ANTHROPIC_BASE_URL` directly -- the
/// real-adapter e2e test uses exactly that. Unlike the `secret` field
/// (which is deliberately never echoed back, only its opaque `KeyRef`),
/// `launch_overrides` was returned byte-for-byte in every
/// `profiles/create`/`update`/`list` response with no redaction at all.
/// For a gateway explicitly designed to serve *multiple concurrent
/// clients* (this workspace's own stated purpose) sharing one
/// `ACPX_AUTH_TOKEN`, that meant any client able to call `profiles/list`
/// could read every other client's raw API keys in plaintext, not just
/// its own. Keys are left visible (so a client can still see *which*
/// vars a profile overrides, useful for debugging) -- only values are
/// masked, mirroring the existing "secret material is never echoed"
/// precedent for `key_ref`/`Keystore`.
fn redact_launch_overrides(mut profile_json: serde_json::Value) -> serde_json::Value {
    if let Some(overrides) = profile_json
        .get_mut("launch_overrides")
        .and_then(|v| v.as_object_mut())
    {
        for value in overrides.values_mut() {
            *value = serde_json::Value::String("***redacted***".to_string());
        }
    }
    profile_json
}

/// Build a `SpawnSpec` for one of the official registry's `npx`-distributed
/// agents (Claude/Codex/Gemini today) -- `npx -y <package> <dist.args...>`.
/// Falls back to `uvx <package> <dist.args...>` when only a `uvx`
/// distribution is declared. Returns `None` for `binary`-only agents --
/// managed-mode profile launches aren't wired to the `binary` install path
/// (Phase 4 step 19) yet; no registry entry Claude/Codex/Gemini use today
/// needs it, per `01-research.md`.
fn npx_spawn_spec(agent: &acpx_registry::Agent) -> Option<acpx_conductor::SpawnSpec> {
    if let Some(npx) = &agent.distribution.npx {
        let mut args = vec!["-y".to_string(), npx.package.clone()];
        args.extend(npx.args.iter().cloned());
        return Some(acpx_conductor::SpawnSpec::new("npx", args));
    }
    if let Some(uvx) = &agent.distribution.uvx {
        let mut args = vec![uvx.package.clone()];
        args.extend(uvx.args.iter().cloned());
        return Some(acpx_conductor::SpawnSpec::new("uvx", args));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_methods() {
        assert_eq!(classify("session/new"), MethodClass::Hybrid);
        assert_eq!(classify("session/prompt"), MethodClass::Proxied);
        assert_eq!(classify("agents/list"), MethodClass::GatewayNative);
        assert_eq!(classify("bogus/method"), MethodClass::Unknown);
    }

    #[test]
    fn classifies_mcp_server_methods_as_gateway_native() {
        assert_eq!(classify("mcp_servers/create"), MethodClass::GatewayNative);
        assert_eq!(classify("mcp_servers/list"), MethodClass::GatewayNative);
        assert_eq!(classify("mcp_servers/update"), MethodClass::GatewayNative);
        assert_eq!(classify("mcp_servers/delete"), MethodClass::GatewayNative);
    }
}
