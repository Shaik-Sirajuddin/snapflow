//! Method classification (gateway-native vs. proxied vs. hybrid) per
//! `02-architecture.md`'s classification table. Phase 1 only needs
//! classification for the single-agent passthrough set; profile
//! resolution, MCP-server merge, and gateway-native handlers land in
//! Phase 2/3.

use crate::keystore::Keystore;
use crate::mcp_servers::McpServerStore;
use crate::notify::NotificationHub;
use crate::persistence::{Direction, PersistenceStore};
use crate::profile::{PermissionPolicy, Profile, ProfileStore};
use crate::provider::ProviderStore;
use crate::session_registry::{BackendSessionId, SessionRegistry};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

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
        // **Phase 6 addition -- closes a real, previously-undiscovered
        // gap:** a spec-compliant ACP client always sends `initialize`
        // as its very first call over the wire, before anything else
        // (per agentclientprotocol.com's own documented handshake
        // flow). Every dispatch path and every test in this workspace
        // before this phase only ever implemented/exercised the
        // *backend*-facing side of `initialize`/`authenticate` (acpx
        // calling out to whatever process a profile spawns -- see
        // `ensure_backend_initialized`); acpx's own client-facing
        // endpoint never classified either method at all, so it fell
        // through to `MethodClass::Unknown` and any real ACP editor/IDE
        // connecting to acpx would get an immediate `UnknownMethod`
        // error on its first ever request, before `session/new` was
        // ever reached. See `dispatch_native`'s `"initialize"`/
        // `"authenticate"` arms for what acpx answers now.
        // **Phase 9 addition:** `logout` is a real, stable v1 ACP method
        // (`agentclientprotocol.com`'s schema: `x-side: agent`, no
        // `sessionId` -- it's connection-scoped, not session-scoped) that
        // was entirely unclassified before this phase, same category as
        // phase 6's pre-fix `initialize`/`authenticate` gap: it fell
        // through to `MethodClass::Unknown` and any client that first
        // checked `agentCapabilities.auth.logout` (correctly, since
        // that's how the spec says to gate calling it at all) would
        // never even try -- but a client that called it anyway got a
        // generic `UnknownMethod` rather than a clear "not supported"
        // answer. Routed `GatewayNative` (not `Proxied`) because it has
        // no `sessionId` to resolve a specific backend from -- in
        // acpx's multi-backend gateway there is no single unambiguous
        // backend a bare `logout` could target, unlike a real
        // single-agent ACP agent where there's exactly one connection.
        // See `dispatch_native`'s `"logout"` arm for what acpx answers.
        "initialize" | "authenticate" | "logout" => MethodClass::GatewayNative,
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
        | "session/set_config_option"
        // **Phase 9 addition:** `session/delete` -- real, stable v1 ACP
        // method (`DeleteSessionRequest`/`DeleteSessionResponse`, `x-side:
        // agent`, carries `sessionId`) found entirely unclassified during
        // this phase's schema recheck (fetched the real `schema/v1/
        // schema.json` off `agentclientprotocol/agent-client-protocol`
        // directly rather than trusting secondary summaries, after phase
        // 8's recheck flagged conflicting claims about its stability).
        // `claude-agent-acp`'s own compiled `dist/acp-agent.js` implements
        // `deleteSession` for real (confirmed by reading it in this
        // phase), so this was a genuine, exercisable gap, not a
        // theoretical one. Session-scoped like `session/close`, so it
        // fits `Proxied` exactly, and shares `rehydrate_session`'s
        // restart-survival fallback with `session/load`/`session/resume`
        // below -- deleting a session a client knows about from a
        // *previous* acpx process lifetime is exactly as legitimate a use
        // case as loading/resuming one.
        | "session/delete" => MethodClass::Proxied,
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

/// **Phase 13 addition.** Which specific backend a dual-mode
/// `session/list` call should be proxied to, per the `_acpx` extension
/// convention already established by `session/new`'s `_acpx.profile`.
/// `Profile` resolves through `Router::resolve_profile` exactly like
/// `session/new`'s managed mode; `AgentId` names an already-registered
/// supervisor key directly (most usefully `default_agent_id`, for
/// native/unmanaged mode, which has no profile at all to name).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionListSelector {
    Profile(String),
    AgentId(String),
}

/// Extracts a [`SessionListSelector`] from a `session/list` call's
/// `params`, if any -- `None` means "no backend selector given," which
/// is what routes the call to acpx's own gateway-scoped aggregate view
/// instead of a real per-backend proxy (see `dispatch_native`'s
/// `"session/list"` arm and `dispatch_shared`'s matching guard, which
/// both call this to decide).
fn session_list_selector(params: &serde_json::Value) -> Option<SessionListSelector> {
    let ext = params.get("_acpx")?;
    if let Some(name) = ext.get("profile").and_then(|p| p.as_str()) {
        return Some(SessionListSelector::Profile(name.to_string()));
    }
    if let Some(id) = ext.get("agentId").and_then(|p| p.as_str()) {
        return Some(SessionListSelector::AgentId(id.to_string()));
    }
    None
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
    /// **Phase 14 addition.** Live `session/update` fan-out to whichever
    /// persistent transport connection (stdio/WS) currently owns a given
    /// gateway session -- see `crate::notify`'s module doc comment for
    /// the full rationale. Cheaply cloneable (an `Arc` internally), so
    /// [`Self::notification_hub`] hands a clone straight to a transport
    /// without that transport ever needing to come back through this
    /// `Router`'s own lock to subscribe/publish.
    notification_hub: NotificationHub,
    /// **Phase 15 addition.** Identity (`Arc::as_ptr` cast to `usize`) of
    /// every physical backend process instance that already has an idle
    /// scavenger task (see [`spawn_idle_scavenger`]/[`backend_idle_
    /// scavenger`]) running for it. Keyed by pointer identity rather than
    /// `agent_id` on purpose: a crash+respawn hands back a brand new
    /// `SharedBackendProcess` (a fresh `Arc`, per `Supervisor::
    /// ensure_running`'s doc comment), which naturally yields a fresh,
    /// not-yet-present key here, so a respawned process always gets its
    /// own fresh scavenger rather than either leaking the crashed
    /// process's now-pointless task forever or requiring any explicit
    /// crash-detection bookkeeping of its own -- the stale task simply
    /// notices its process has exited (`BackendProcess::has_exited`) and
    /// returns on its own next tick, see that function's doc comment.
    scavenged_backends: HashSet<usize>,
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
    #[error("backend requires authentication before session/new (advertised authMethods: {0}); configure Profile::auth_method_id to pick one")]
    BackendRequiresAuthentication(serde_json::Value),
    #[error("backend rejected authenticate: {0}")]
    BackendAuthenticationError(serde_json::Value),
    #[error("authenticate: acpx's own initialize response advertises no authMethods (requested methodId: {0:?}); no transport-level bearer-token/session auth is bypassed by this -- see acpx-server's own HTTP/WS auth")]
    NoAuthMethodsAdvertised(Option<String>),
    #[error(
        "session/load: gateway session {0} not found in this process's live registry and \
         no persistence store is configured to recover it from -- pass ACPX_DB_PATH so \
         session/load can survive an acpx restart"
    )]
    SessionNotPersisted(String),
    #[error(
        "session/load: gateway session {0} could not be recovered from the persistence store: {1}"
    )]
    SessionRehydrationFailed(String, crate::persistence::PersistenceError),
    #[error(
        "logout: acpx's own initialize response advertises no agentCapabilities.auth.logout \
         (gateway-level auth is transport-level, not ACP-level -- see acpx-server's own \
         HTTP/WS auth); acpx also has no single unambiguous backend a bare, session-less \
         logout could target across its multiple managed profiles"
    )]
    LogoutNotSupported,
    #[error("backend rejected session/list: {0}")]
    BackendSessionListError(serde_json::Value),
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
            notification_hub: NotificationHub::new(),
            scavenged_backends: HashSet::new(),
        }
    }

    /// A clone of this router's live `session/update` notification hub
    /// (Phase 14) -- `acpx-server`'s stdio/WS transports call this once
    /// per connection to subscribe to whichever gateway sessions that
    /// connection touches. See `crate::notify`'s module doc comment.
    pub fn notification_hub(&self) -> NotificationHub {
        self.notification_hub.clone()
    }

    /// **Phase 15.** Ensure exactly one idle scavenger task
    /// ([`backend_idle_scavenger`]) is running for this exact physical
    /// `backend` instance, spawning one the first time this backend is
    /// ever seen and doing nothing on every later call against the same
    /// still-running process. Called from every `_shared` dispatch path
    /// right after `Supervisor::ensure_running` hands back a
    /// `SharedBackendProcess`, while that call already holds `self`'s own
    /// lock briefly for bookkeeping -- spawning a task is not backend
    /// I/O, so doing it here doesn't violate this file's "release the
    /// lock before any backend round trip" convention.
    fn spawn_idle_scavenger_if_new(
        &mut self,
        router_handle: &SharedRouterHandle,
        agent_id: &str,
        backend: &acpx_conductor::supervisor::SharedBackendProcess,
    ) {
        let key = std::sync::Arc::as_ptr(backend) as usize;
        if !self.scavenged_backends.insert(key) {
            return;
        }
        let ctx = LiveNotifyCtx {
            router: std::sync::Arc::clone(router_handle),
            agent_id: agent_id.to_string(),
        };
        let backend = std::sync::Arc::clone(backend);
        tokio::spawn(backend_idle_scavenger(backend, ctx));
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

        // **Phase 13 addition.** Captured before any further mutation
        // below (the `_acpx` strip and `mcpServers` merge never touch
        // `cwd`) so it can be threaded into `SessionEntry::cwd` --
        // real per-backend `session/list`'s `SessionInfo.cwd` is a
        // *required* field, so acpx's own gateway-scoped aggregate needs
        // to actually know it to include it honestly.
        let cwd = params
            .get("cwd")
            .and_then(|c| c.as_str())
            .map(str::to_string);

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
        let call_policy = BackendCallPolicy::from_profile(profile.as_ref());
        let mut response = {
            let mut backend = backend.lock().await;
            ensure_backend_initialized(&mut backend, call_policy.clone()).await?;
            backend.writer.lock().await.write_value(&request).await?;
            let (response, notifications, agent_requests) =
                read_matching_response(&mut backend, &id, call_policy, None).await?;
            attach_session_new_extras(
                response,
                notifications,
                agent_requests,
                backend.agent_capabilities.clone(),
            )
        };

        let backend_session_id = extract_backend_session_id(&response)?;
        let gateway_id = self.sessions.register(
            agent_id,
            BackendSessionId(backend_session_id),
            profile.as_ref().map(|p| p.name.clone()),
            cwd,
        );

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

    /// **Phase 13 addition.** The real, spec-shaped half of dual-mode
    /// `session/list` (see `dispatch_native`'s `"session/list"` arm for
    /// the branching, and `session_list_selector`/`SessionListSelector`
    /// for how a client opts into this path). Resolves the requested
    /// backend exactly like `dispatch_session_new` does (`_acpx.profile`
    /// via `resolve_profile`, or a raw `_acpx.agentId` naming an
    /// already-registered supervisor key directly -- e.g. `default_agent_id`
    /// in native/unmanaged mode), forwards a real `session/list` request
    /// (params minus `_acpx`) to that one backend, and translates every
    /// returned `SessionInfo.sessionId` from the backend's own native id
    /// into a gateway id via `translate_or_register_backend_session` --
    /// without that translation step the response would hand the client
    /// ids it could never use against any other acpx method again,
    /// defeating the entire point of listing sessions through a proxy in
    /// the first place.
    async fn dispatch_session_list_real(
        &mut self,
        id: serde_json::Value,
        selector: SessionListSelector,
        mut params: serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        if let Some(obj) = params.as_object_mut() {
            obj.remove("_acpx");
        }
        let (agent_id, profile) = match selector {
            SessionListSelector::Profile(name) => {
                let (key, profile) = self.resolve_profile(&name).await?;
                (key, Some(profile))
            }
            SessionListSelector::AgentId(explicit_id) => (explicit_id, None),
        };
        let profile_name = profile.as_ref().map(|p| p.name.clone());
        let call_policy = BackendCallPolicy::from_profile(profile.as_ref());
        let backend = self.supervisor.ensure_running(&agent_id).await?;

        let outbound = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/list",
            "params": params,
        });

        let response = {
            let mut proc = backend.lock().await;
            ensure_backend_initialized(&mut proc, call_policy.clone()).await?;
            proc.writer.lock().await.write_value(&outbound).await?;
            let (response, _notifications, _agent_requests) =
                read_matching_response(&mut proc, &id, call_policy, None).await?;
            response
        };

        if let Some(error) = response.get("error") {
            return Err(RouterError::BackendSessionListError(error.clone()));
        }

        let mut result = response
            .get("result")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "sessions": [] }));
        if let Some(sessions) = result.get_mut("sessions").and_then(|s| s.as_array_mut()) {
            for session in sessions.iter_mut() {
                let Some(backend_sid) = session
                    .get("sessionId")
                    .and_then(|s| s.as_str())
                    .map(str::to_string)
                else {
                    continue;
                };
                let session_cwd = session
                    .get("cwd")
                    .and_then(|c| c.as_str())
                    .map(str::to_string);
                let gateway_id = self.translate_or_register_backend_session(
                    &agent_id,
                    &backend_sid,
                    profile_name.clone(),
                    session_cwd,
                );
                session["sessionId"] = serde_json::Value::String(gateway_id.clone());
                self.spawn_session_persistence(
                    gateway_id,
                    agent_id.clone(),
                    backend_sid,
                    profile_name.clone(),
                    outbound.clone(),
                    response.clone(),
                );
            }
        }
        Ok(result)
    }

    /// See [`Self::dispatch_session_list_real`]'s doc comment and
    /// `SessionRegistry::find_by_backend`'s. Reuses an already-known
    /// gateway id for this exact `(agent_id, backend_session_id)` pair if
    /// one exists (e.g. a session acpx itself opened earlier in this
    /// process's lifetime via `session/new`); otherwise mints and
    /// registers a fresh one on the spot -- the same "recover a backend
    /// session into the live registry" move `rehydrate_session` makes for
    /// `session/load`, just triggered by discovery through `session/list`
    /// rather than an explicit client-supplied gateway id. From this
    /// point on the returned id is a first-class gateway session,
    /// `session/load`-able (and, once loaded, promptable) exactly like
    /// any other -- **the concrete, testable proof this isn't just a
    /// cosmetic id swap.**
    fn translate_or_register_backend_session(
        &mut self,
        agent_id: &str,
        backend_session_id: &str,
        profile_name: Option<String>,
        cwd: Option<String>,
    ) -> String {
        if let Some(existing) = self.sessions.find_by_backend(agent_id, backend_session_id) {
            return existing.0;
        }
        self.sessions
            .register(
                agent_id.to_string(),
                BackendSessionId(backend_session_id.to_string()),
                profile_name,
                cwd,
            )
            .0
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

    /// **Phase 8 addition -- closes a real gap.** `session/load` (and its
    /// close cousin `session/resume`) exist in the real ACP spec
    /// specifically so a client can resume a session it learned about
    /// through some *other* channel than "I just called `session/new` in
    /// this exact process's lifetime" -- most obviously, reconnecting
    /// after the agent process (here, acpx itself) restarted and its
    /// in-memory `SessionRegistry` was wiped clean. Before this phase,
    /// every `Proxied` method -- `session/load` included -- required the
    /// gateway session id to already be a live key in that in-memory
    /// map, which made `session/load` in this gateway strictly *less*
    /// capable than in a real single-agent ACP agent: it could only ever
    /// re-request an already-open session, never genuinely resume one
    /// that outlived acpx's own process. That defeats the entire purpose
    /// of the method existing separately from `session/new`.
    ///
    /// This only fires as a fallback (the in-memory registry is always
    /// checked first, unchanged, by both call sites) and only for
    /// `session/load`/`session/resume` specifically -- every other
    /// `Proxied` method (`session/prompt`, `session/cancel`, etc.) still
    /// requires a live in-process session and correctly errors
    /// `UnknownSession` otherwise; those aren't resumption calls, so
    /// silently reviving one from a stale durable row on, say, a typo'd
    /// `session/prompt` call would paper over a real client bug instead
    /// of surfacing it.
    ///
    /// Requires `ACPX_DB_PATH`/`Router::with_persistence` to have been
    /// configured; without it there is nowhere durable to recover from,
    /// so this errors clearly (`SessionNotPersisted`) rather than
    /// silently behaving as if the session never existed at all vs.
    /// "recovery wasn't even possible here" -- the two are genuinely
    /// different failure modes worth distinguishing for whoever reads
    /// the error.
    async fn rehydrate_session(
        &mut self,
        method: &str,
        gateway_session_id: &str,
    ) -> Result<crate::session_registry::SessionEntry, RouterError> {
        if !matches!(method, "session/load" | "session/resume" | "session/delete") {
            return Err(RouterError::UnknownSession(gateway_session_id.to_string()));
        }
        let store = self
            .persistence
            .clone()
            .ok_or_else(|| RouterError::SessionNotPersisted(gateway_session_id.to_string()))?;
        let record = store
            .get_session(gateway_session_id.to_string())
            .await
            .map_err(|err| {
                RouterError::SessionRehydrationFailed(gateway_session_id.to_string(), err)
            })?
            .ok_or_else(|| RouterError::UnknownSession(gateway_session_id.to_string()))?;
        let entry = crate::session_registry::SessionEntry {
            agent_id: record.agent_id,
            backend_session_id: BackendSessionId(record.backend_session_id),
            profile_name: record.profile_name,
            // The `sessions` sqlite table doesn't carry `cwd` yet (see
            // `SessionEntry::cwd`'s doc comment) -- honestly `None` here
            // rather than guessing, not a regression this phase
            // introduced.
            cwd: None,
        };
        // **The real, second half of this bug.** `entry.agent_id` here is
        // actually the *supervisor key* `profile:{name}` minted by
        // `resolve_profile`/`dispatch_session_new` at the time this
        // session was first created -- not a raw registry agent id. This
        // process's own `Supervisor` has never seen that key before (it
        // never ran the `session/new` that originally registered it), so
        // `ensure_running` would otherwise fail with "no spawn spec
        // registered for agent <key>" even though the session row itself
        // resolved correctly. Re-running `resolve_profile` (idempotent --
        // it just re-registers the same `SpawnSpec` under the same key,
        // exactly like every ordinary `session/new` call already does)
        // fixes that; a `None` `profile_name` (native/unmanaged mode)
        // needs no such step since `default_agent_id`'s spec is already
        // registered unconditionally at process startup. Caught by
        // `ambient_claude_session_load_survives_a_real_gateway_restart`
        // actually spawning a *second*, independent `acpx-server`
        // process -- an in-process-only test would never have exercised
        // a `Supervisor` that legitimately never saw this profile before.
        if let Some(name) = entry.profile_name.as_deref() {
            self.resolve_profile(name).await?;
        }
        self.sessions.insert(
            acpx_proto::session::GatewaySessionId(gateway_session_id.to_string()),
            entry.clone(),
        );
        Ok(entry)
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
        // **Phase 7:** `session/cancel` is not shaped like every other
        // `Proxied` method -- see `Self::dispatch_session_cancel`'s doc
        // comment for the two real bugs this branch closes (a
        // spec-compliant client's notification-shaped call, with no
        // `id`, getting rejected by the generic `MissingId` check below;
        // and the generic path's blocking wait for a reply the backend
        // is never supposed to send). Must be checked before that `id`
        // extraction, not after.
        if method == "session/cancel" {
            return self.dispatch_session_cancel(request).await;
        }
        let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;
        let params = request
            .get_mut("params")
            .ok_or(RouterError::MissingParams)?;
        let gateway_session_id = params
            .get("sessionId")
            .and_then(|s| s.as_str())
            .ok_or(RouterError::MissingSessionId)?
            .to_string();

        let entry = match self
            .sessions
            .resolve(&acpx_proto::session::GatewaySessionId(
                gateway_session_id.clone(),
            )) {
            Some(entry) => entry.clone(),
            None => self.rehydrate_session(&method, &gateway_session_id).await?,
        };
        let agent_id = entry.agent_id.clone();
        let backend_session_id = entry.backend_session_id.0.clone();
        let profile_name = entry.profile_name.clone();
        let call_policy = BackendCallPolicy::from_profile(
            profile_name
                .as_deref()
                .and_then(|name| self.profiles.get(name)),
        );

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
            ensure_backend_initialized(&mut backend, call_policy.clone()).await?;
            backend.writer.lock().await.write_value(&request).await?;
            let (response, notifications, agent_requests) =
                read_matching_response(&mut backend, &id, call_policy, None).await?;
            attach_updates(response, notifications, agent_requests)
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

    /// Real ACP `session/cancel` -- a client-sent *notification* per
    /// `agentclientprotocol.com`'s `CancelNotification` schema: no `id`,
    /// no reply expected on the wire for it directly. The agent's only
    /// observable reaction is that the already-in-flight `session/prompt`
    /// call it's meant to interrupt eventually resolves with
    /// `stopReason: "cancelled"` on its own -- that resolution flows back
    /// through the *original* `session/prompt` call's own already-running
    /// `dispatch_proxied`/`dispatch_proxied_shared` invocation, not
    /// through anything this method does.
    ///
    /// **Two real, previously-undiscovered bugs this closes** (found
    /// re-deriving the ACP spec surface for phase 7's recheck, not from a
    /// test failure -- this workspace had zero tests exercising
    /// `session/cancel` at all before this phase, despite it being one of
    /// four methods the spec calls out as a baseline MUST for every
    /// agent): (1) every other `Proxied` method unconditionally required
    /// an `id` (`RouterError::MissingId` otherwise) -- a spec-compliant
    /// client sending this as a true notification (no `id` at all) would
    /// have been rejected before the request ever reached a backend; (2)
    /// the generic proxied path blocks on `read_matching_response`
    /// waiting for a reply carrying the forwarded request's own id -- a
    /// spec-compliant backend never replies to `session/cancel` directly,
    /// so that would hang forever: a real deadlock against any
    /// correctly-implemented backend, not a hypothetical. Same category
    /// of bug as phase 2's `session/request_permission` fix, in the
    /// opposite direction: there, an agent-initiated *request* was
    /// mistaken for a notification; here, a client-sent *notification*
    /// was mistaken for a request awaiting a reply.
    ///
    /// **Third, deeper bug this closes -- the reason this isn't just a
    /// shape fix:** even with (1)/(2) fixed, routing this through the
    /// same per-process lock every other proxied method uses
    /// (`SharedBackendProcess`'s `Arc<Mutex<BackendProcess>>`) would
    /// still leave cancellation practically useless -- a `session/prompt`
    /// call already in flight against this exact backend process holds
    /// that lock for its *entire* duration (the whole point of the
    /// "real multi-agent concurrency" design), so a cancel routed through
    /// it could only ever be delivered *after* the very call it's meant
    /// to interrupt has already finished, at which point cancelling is
    /// moot. This writes through
    /// `acpx_conductor::supervisor::Supervisor::cancel_writer` instead --
    /// a handle independent of that per-process lock (see its and
    /// `BackendProcess::writer`'s doc comments) -- so the notification
    /// genuinely reaches the backend's stdin *while* the in-flight call
    /// is still blocked reading, not only after.
    ///
    /// Writes the real ACP notification shape verbatim
    /// (`{jsonrpc, method, params: {sessionId}}`, deliberately no `id`
    /// key at all, regardless of whatever shape the client's own call
    /// used) and returns immediately once that write succeeds, echoing
    /// the client's own `id` back (or `null` if the client sent a true
    /// notification) -- acpx's own client-facing transports are all
    /// request/response-shaped regardless of what ACP itself calls this
    /// method, so some reply is always sent, but nothing about it is a
    /// real backend acknowledgment (there isn't one to wait for).
    async fn dispatch_session_cancel(
        &mut self,
        request: serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        let client_id = request
            .get("id")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let gateway_session_id = request
            .get("params")
            .and_then(|p| p.get("sessionId"))
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

        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": backend_session_id }
        });
        self.spawn_transcript(
            gateway_session_id,
            Direction::ClientToAgent,
            notification.clone(),
        );

        // `None` means this agent's process was never spawned (or was
        // `stop`ped) -- nothing is in flight to cancel, a benign no-op
        // rather than an error: a client cancelling a session whose
        // backend isn't even running has, definitionally, nothing left
        // to interrupt.
        if let Some(writer) = self.supervisor.cancel_writer(&agent_id) {
            writer.lock().await.write_value(&notification).await?;
        }

        Ok(serde_json::json!({ "jsonrpc": "2.0", "id": client_id, "result": {} }))
    }

    async fn dispatch_native(
        &mut self,
        method: &str,
        request: serde_json::Value,
    ) -> Result<serde_json::Value, RouterError> {
        let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;
        let result = match method {
            // **Phase 6:** acpx's own client-facing `initialize` --
            // distinct from `ensure_backend_initialized`'s backend-facing
            // handshake (that one negotiates with whatever process a
            // profile spawns; this one is acpx itself answering as the
            // ACP agent its clients think they're talking to). Real
            // schema defaults confirmed against agentclientprotocol.com/
            // protocol/schema: `agentCapabilities` defaults to
            // `{loadSession:false, mcpCapabilities:{http:false,sse:false},
            // promptCapabilities:{audio:false,embeddedContext:false,
            // image:false}}`, `authMethods` to `[]`. acpx declares the
            // permissive end of each flag instead of the spec's
            // conservative defaults: as a transparent multiplexing proxy,
            // acpx itself never inspects, transforms, or strips prompt
            // content blocks, `mcpServers` transport kinds, or
            // `session/load` calls -- it forwards every one of them
            // verbatim to whichever backend a later `session/new`
            // resolves to (see `classify`'s `Proxied`/`Hybrid` buckets),
            // so it imposes no restriction of its own to advertise here.
            // This is honestly *not* a promise that every backend a
            // client might later select via `_acpx.profile` actually
            // supports all of these -- that per-backend truth is only
            // knowable after `session/new`, and already surfaced there
            // via `_acpx.agentCapabilities` (phase 1). `authMethods`
            // stays the spec default `[]`: acpx-server's own access
            // control is transport-level (HTTP bearer token / WS auth,
            // enforced before a request ever reaches this dispatcher),
            // not an ACP-level `authenticate` exchange, so there is
            // genuinely no method id to advertise.
            "initialize" => serde_json::json!({
                "protocolVersion": 1,
                "agentCapabilities": {
                    "loadSession": true,
                    "promptCapabilities": {
                        "image": true,
                        "audio": true,
                        "embeddedContext": true
                    },
                    "mcpCapabilities": {
                        "http": true,
                        "sse": true
                    },
                    // **Phase 9 addition, `list` added phase 13.** Per
                    // the real v1 stable schema's `SessionCapabilities`,
                    // advertises `close`/`delete`/`resume`/`list` as
                    // supported -- honest, because all four are
                    // genuinely forwarded to whatever real backend a
                    // caller selects (see `classify` for `close`/
                    // `delete`/`resume`, and `dispatch_native`'s
                    // `"session/list"` arm plus `session_list_selector`
                    // for `list`'s dual-mode split). `list` specifically:
                    // phase 9 through 12 deliberately omitted it because
                    // acpx's own `session/list` answered *only* from its
                    // gateway-scoped `SessionRegistry` (no `cwd`, no
                    // per-backend `SessionInfo[]` shape at all) -- a
                    // genuine, tracked divergence from the real schema.
                    // Phase 13 closed that: `session/list` now forwards
                    // to a real backend's own `session/list` (translating
                    // returned session ids into gateway ids so they stay
                    // usable through acpx afterward) whenever the caller
                    // supplies the same `_acpx` backend-selector
                    // convention `session/new`'s `_acpx.profile` already
                    // established; an unqualified call keeps answering
                    // the gateway-wide aggregate instead (no single real
                    // backend could ever honestly answer *that* question
                    // -- it's the entire reason a multiplexing gateway
                    // exists), which is why this capability flag is an
                    // honest "can be spec-conformant, not that every
                    // unqualified call is" the same way `loadSession`/
                    // `promptCapabilities` above already are for their
                    // own per-backend caveats. `additionalDirectories` is
                    // still omitted: acpx forwards whatever a client
                    // sends verbatim but never itself inspects/validates
                    // that field, so there's no acpx-level claim to make
                    // about it either way.
                    "sessionCapabilities": {
                        "close": {},
                        "delete": {},
                        "resume": {},
                        "list": {}
                    }
                },
                "authMethods": [],
                "agentInfo": {
                    "name": "acpx",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
            // **Phase 6:** a compliant client only ever calls
            // `authenticate` in response to a non-empty `authMethods` in
            // `initialize`'s result -- acpx's own `initialize` (just
            // above) always advertises `[]`, so any `authenticate` call
            // acpx receives here is, by definition, requesting a method
            // id that was never offered. Reply with a clear, specific
            // error rather than either silently succeeding (which would
            // misrepresent that some real authentication happened) or a
            // bare method-not-found (which would misrepresent
            // `authenticate` itself as unsupported, when it's the
            // *methodId* that's the problem).
            "authenticate" => {
                let method_id = request
                    .get("params")
                    .and_then(|p| p.get("methodId"))
                    .and_then(|m| m.as_str())
                    .map(str::to_string);
                return Err(RouterError::NoAuthMethodsAdvertised(method_id));
            }
            // **Phase 9 addition**, same reasoning as `authenticate`
            // just above: acpx's own `initialize` response deliberately
            // never sets `agentCapabilities.auth.logout` (omitted
            // entirely, meaning "not supported" per the real schema's
            // own stated default), because acpx-server's own access
            // control is transport-level (HTTP bearer token / WS auth)
            // and there is no ACP-level authenticated state at the
            // *gateway* layer for a client-facing `logout` to
            // meaningfully terminate -- forwarding it to some arbitrary
            // one backend among potentially many active profiles would
            // be actively misleading (which one?), and silently
            // succeeding as a no-op would misrepresent that something
            // real happened. A compliant client checks the capability
            // before calling; one that calls anyway gets a clear,
            // specific error instead of a bare method-not-found.
            "logout" => {
                return Err(RouterError::LogoutNotSupported);
            }
            // **Phase 13.** `session/list` is dual-mode, distinguished by
            // whether the client supplies a backend selector via the
            // established `_acpx` extension convention (same one
            // `session/new` already uses for `_acpx.profile`): with a
            // selector (`_acpx.profile` or `_acpx.agentId`), this is a
            // real, spec-shaped `Proxied` forward to that one specific
            // backend's own `session/list` (see
            // `Self::dispatch_session_list_real`); without one, it stays
            // acpx's original gateway-scoped aggregate view across every
            // backend this process manages -- the reason a multiplexing
            // gateway is worth having in the first place, and something
            // no single real backend's `session/list` could ever answer
            // on its own. See `COVERAGE.md`'s phase 13 entry for the
            // full rationale on why this is a split, not a rename or a
            // one-or-the-other tradeoff.
            "session/list" => {
                let params = request
                    .get("params")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                match session_list_selector(&params) {
                    Some(selector) => {
                        self.dispatch_session_list_real(id.clone(), selector, params)
                            .await?
                    }
                    None => {
                        let sessions: Vec<serde_json::Value> = self
                            .sessions
                            .list()
                            .map(|(gateway_id, entry)| {
                                serde_json::json!({
                                    "sessionId": gateway_id,
                                    "agentId": entry.agent_id,
                                    "cwd": entry.cwd,
                                })
                            })
                            .collect();
                        serde_json::json!({ "sessions": sessions })
                    }
                }
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

/// Fixed request id for the ACP `authenticate` round trip performed
/// against a backend process that advertised a non-empty `authMethods`
/// in its `initialize` response -- same rationale as
/// `INITIALIZE_REQUEST_ID` (numeric, for the synthetic `sh -c '...'`
/// stand-in backends' regex-based id echo), distinct value so a test
/// double can tell the two handshake requests apart if it needs to.
const AUTHENTICATE_REQUEST_ID: i64 = -1;

/// Everything about how `read_matching_response`/`ensure_backend_initialized`
/// should answer a backend's mid-call agent-initiated requests on a given
/// call's behalf, bundled so the growing set of "what is this profile
/// allowed to auto-decide" knobs doesn't turn into an ever-longer
/// parameter list at every one of the four dispatch call sites. Computed
/// once per call from the resolved `Profile` (or defaulted for
/// native/unmanaged mode, where there is no profile to consult at all).
#[derive(Debug, Clone, Default)]
struct BackendCallPolicy {
    permission_policy: PermissionPolicy,
    allow_fs_access: bool,
    allow_terminal_access: bool,
    auth_method_id: Option<String>,
}

impl BackendCallPolicy {
    fn from_profile(profile: Option<&Profile>) -> Self {
        match profile {
            Some(p) => Self {
                permission_policy: p.permission_policy,
                allow_fs_access: p.allow_fs_access,
                allow_terminal_access: p.allow_terminal_access,
                auth_method_id: p.auth_method_id.clone(),
            },
            None => Self::default(),
        }
    }
}

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
/// **Phase 5 addition:** after the `initialize` handshake (whether
/// performed just now or previously, per `handshake_done`), also check
/// the backend's cached `authMethods` (from `proc.agent_capabilities`,
/// so this never re-sends `initialize` on a retry) and drive a real ACP
/// `authenticate` round trip if the backend requires one and hasn't
/// already succeeded (`proc.authenticated`). No pre-configured
/// `Profile::auth_method_id` -- or a backend that rejects the one
/// configured -- surfaces as a clear [`RouterError`] instead of acpx
/// either guessing a method id or silently proceeding to `session/new`
/// and letting the backend's own downstream rejection stand in for a
/// real error message about *why*.
async fn ensure_backend_initialized(
    proc: &mut acpx_conductor::BackendProcess,
    call_policy: BackendCallPolicy,
) -> Result<(), RouterError> {
    if !proc.handshake_done {
        let allow_fs_access = call_policy.allow_fs_access;
        let allow_terminal_access = call_policy.allow_terminal_access;
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": INITIALIZE_REQUEST_ID,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": {
                    // Real -- not aspirational -- as of ACP compatibility
                    // hardening phase 3: `read_matching_response` genuinely
                    // implements both methods now (real disk I/O against
                    // acpx's own host filesystem) when `allow_fs_access` is
                    // `true` for the profile this process belongs to. `false`
                    // (the default -- see `Profile::allow_fs_access`'s doc
                    // comment for why opt-in, not opt-out) keeps declaring
                    // both `false`, byte-for-byte the pre-phase-3 behavior.
                    "fs": { "readTextFile": allow_fs_access, "writeTextFile": allow_fs_access },
                    // Phase 4: same treatment for the `terminal` capability
                    // group -- all five sub-methods tied to one profile-level
                    // opt-in (`Profile::allow_terminal_access`), since
                    // granular per-sub-method opt-in has no real security
                    // value (they're meaningless without each other).
                    "terminal": {
                        "create": allow_terminal_access,
                        "output": allow_terminal_access,
                        "waitForExit": allow_terminal_access,
                        "kill": allow_terminal_access,
                        "release": allow_terminal_access
                    }
                }
            }
        });
        proc.writer.lock().await.write_value(&request).await?;
        loop {
            let value = proc.reader.read_value().await?;
            if value.get("id").and_then(|v| v.as_i64()) == Some(INITIALIZE_REQUEST_ID) {
                // Capture the backend's real `initialize` result -- its
                // actual `agentCapabilities`/`authMethods`/negotiated
                // `protocolVersion` -- instead of discarding it. Surfaced to
                // gateway clients via `session/new`'s `_acpx.agentCapabilities`
                // (see `attach_session_new_extras`) so a client can find out
                // what a given backend genuinely supports rather than acpx
                // silently assuming. **Real gap this closes** (found during
                // an ACP-compatibility self-review, not from a test failure):
                // every dispatch path before this fix threw the `initialize`
                // response away entirely once the id matched, so acpx never
                // knew -- and never told a client -- whether a backend
                // supports e.g. `loadSession`, image content, or any auth
                // method at all.
                proc.agent_capabilities = value.get("result").cloned();
                break;
            }
            // A well-behaved adapter shouldn't emit anything unprompted
            // before answering `initialize`, but stay defensive rather than
            // assuming the very first line back is necessarily the match --
            // `read_value`'s own `FramingError::Eof` on a closed pipe is
            // still the hard stop if the backend never answers at all.
        }
        proc.handshake_done = true;
    }

    // Real ACP `authenticate` -- driven off the *cached* `initialize`
    // result (`proc.agent_capabilities`), not a re-send of `initialize`
    // itself, so this branch is safe to re-run on every call until it
    // succeeds (e.g. after an operator fixes a misconfigured profile)
    // without ever sending a second `initialize` on the same process,
    // which a real adapter has no obligation to tolerate.
    if !proc.authenticated {
        let auth_methods = proc
            .agent_capabilities
            .as_ref()
            .and_then(|r| r.get("authMethods"))
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();
        if !auth_methods.is_empty() {
            let Some(method_id) = call_policy.auth_method_id.as_deref() else {
                return Err(RouterError::BackendRequiresAuthentication(
                    serde_json::Value::Array(auth_methods),
                ));
            };
            let request = serde_json::json!({
                "jsonrpc": "2.0",
                "id": AUTHENTICATE_REQUEST_ID,
                "method": "authenticate",
                "params": { "methodId": method_id }
            });
            proc.writer.lock().await.write_value(&request).await?;
            loop {
                let value = proc.reader.read_value().await?;
                if value.get("id").and_then(|v| v.as_i64()) == Some(AUTHENTICATE_REQUEST_ID) {
                    if let Some(error) = value.get("error") {
                        return Err(RouterError::BackendAuthenticationError(error.clone()));
                    }
                    proc.authenticated = true;
                    break;
                }
                // Same defensive stance as the `initialize` loop above --
                // a well-behaved adapter shouldn't emit anything
                // unprompted before answering `authenticate` either.
            }
        } else {
            // No auth required at all -- vacuously "authenticated" so
            // this branch short-circuits on every subsequent call
            // without re-deriving `auth_methods` from JSON each time.
            proc.authenticated = true;
        }
    }

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

/// Build the client's real ACP `session/request_permission` reply for a
/// given `policy` -- the schema is `agentclientprotocol.com/protocol/
/// schema`'s `RequestPermissionResponse`: `result.outcome` is a
/// discriminated union, either `{"outcome": "selected", "optionId": ..}`
/// or `{"outcome": "cancelled"}`. See [`crate::profile::PermissionPolicy`]'s
/// doc comment for why acpx answers this automatically per profile
/// config rather than leaving it unanswered (ACP's own spec explicitly
/// allows this).
fn build_permission_reply(
    request: &serde_json::Value,
    policy: PermissionPolicy,
) -> serde_json::Value {
    let id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let options: Vec<serde_json::Value> = request
        .get("params")
        .and_then(|p| p.get("options"))
        .and_then(|o| o.as_array())
        .cloned()
        .unwrap_or_default();
    let kind_prefix = match policy {
        PermissionPolicy::AutoAllow => "allow_",
        PermissionPolicy::AutoReject => "reject_",
    };
    let by_kind = options.iter().find(|opt| {
        opt.get("kind")
            .and_then(|k| k.as_str())
            .map(|k| k.starts_with(kind_prefix))
            .unwrap_or(false)
    });
    // `AutoAllow` falls back to the backend's first offered option if
    // none is explicitly labeled `allow_*` (matching the reference Go
    // SDK's own "no preference -> first option" behavior) -- this policy
    // is already an explicit opt-in to acpx deciding "yes" on the
    // client's behalf. `AutoReject` never does the equivalent fallback:
    // guessing an unlabeled option under the *safety-conservative*
    // default policy could easily select something that isn't actually a
    // rejection, so it replies `cancelled` instead when no `reject_*`
    // option was offered.
    let chosen = by_kind.or_else(|| match policy {
        PermissionPolicy::AutoAllow => options.first(),
        PermissionPolicy::AutoReject => None,
    });
    let outcome = match chosen.and_then(|opt| opt.get("optionId").and_then(|o| o.as_str())) {
        Some(option_id) => serde_json::json!({"outcome": "selected", "optionId": option_id}),
        None => serde_json::json!({"outcome": "cancelled"}),
    };
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {"outcome": outcome}
    })
}

/// Answer a real `fs/read_text_file` or `fs/write_text_file` request
/// against acpx's own host filesystem -- real disk I/O, not a stub.
/// Schema per `agentclientprotocol.com/protocol/file-system`:
/// `fs/read_text_file`'s params are `{sessionId, path, line?, limit?}`
/// (`line`: 1-indexed line to start from; `limit`: max number of lines),
/// result `{content}`; `fs/write_text_file`'s params are
/// `{sessionId, path, content}`, result `{}` (no data, success is the
/// signal). `path` is used exactly as sent -- real ACP clients (editors)
/// always send absolute paths, and acpx has no separate notion of a
/// session's own "workspace root" to resolve a relative one against
/// today, so a backend sending a relative path gets whatever
/// `std::env::current_dir` resolves it against, same as any other
/// process. Only reached when `allow_fs_access` is already `true` for
/// the calling profile -- callers must check that first (see
/// `read_matching_response`), since this function does no permission
/// check of its own.
async fn handle_fs_request(request: &serde_json::Value, method: &str) -> serde_json::Value {
    let id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let params = request.get("params");
    let path = match params.and_then(|p| p.get("path")).and_then(|p| p.as_str()) {
        Some(path) => path.to_string(),
        None => {
            return serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32602, "message": "missing required 'path' param"}
            })
        }
    };
    match method {
        "fs/read_text_file" => {
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => {
                    let line = params.and_then(|p| p.get("line")).and_then(|l| l.as_u64());
                    let limit = params.and_then(|p| p.get("limit")).and_then(|l| l.as_u64());
                    let content = match (line, limit) {
                        (None, None) => content,
                        (line, limit) => {
                            // `line` is 1-indexed per the ACP schema; absent
                            // means start from the top. `limit` caps the
                            // number of lines returned; absent means the
                            // rest of the file.
                            let start = line.map(|l| l.saturating_sub(1) as usize).unwrap_or(0);
                            let lines: Vec<&str> = content.lines().collect();
                            let end = match limit {
                                Some(n) => (start + n as usize).min(lines.len()),
                                None => lines.len(),
                            };
                            lines.get(start..end).unwrap_or(&[]).join("\n")
                        }
                    };
                    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"content": content}})
                }
                Err(err) => serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32001, "message": format!("fs/read_text_file: {err}"), "data": {"path": path}}
                }),
            }
        }
        "fs/write_text_file" => {
            let content = params
                .and_then(|p| p.get("content"))
                .and_then(|c| c.as_str())
                .unwrap_or("");
            match tokio::fs::write(&path, content).await {
                Ok(()) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                Err(err) => serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32001, "message": format!("fs/write_text_file: {err}"), "data": {"path": path}}
                }),
            }
        }
        // Unreachable in practice -- `read_matching_response` only calls
        // this for the two methods above -- but a `match` without a
        // catch-all here would be a silent trap for a future third `fs/*`
        // method added to that call site without updating this function.
        other => serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": format!("acpx gateway does not implement '{other}'")}
        }),
    }
}

/// Answer a real `terminal/*` request against acpx's own host, backed by
/// `acpx_conductor::TerminalHandle` (see that module's doc comment).
/// Schema per `agentclientprotocol.com/protocol/v1/terminals`:
/// `terminal/create`'s params are `{sessionId, command, args?, env?,
/// cwd?, outputByteLimit?}` (`env` is ACP's usual array-of-`{name,value}`
/// shape, not a JSON object map) -> `{terminalId}`; `terminal/output` ->
/// `{output, truncated, exitStatus?}` (`truncated` is a **required**
/// field per the real schema -- phase 10 fix, was silently omitted
/// before); `terminal/wait_for_exit` -> `{exitStatus}`;
/// `terminal/kill`/`terminal/release` -> `{}`. `exitStatus` is
/// `{exitCode, signal}` (either may be `null`). Needs `&mut proc` (unlike
/// `handle_fs_request`) since terminal state lives in
/// `BackendProcess::terminals`, keyed by the terminal id acpx mints in
/// `terminal/create`'s reply and the backend passes back on every
/// subsequent call. Only reached when `allow_terminal_access` is already
/// `true` for the calling profile -- see `read_matching_response`.
async fn handle_terminal_request(
    proc: &mut acpx_conductor::BackendProcess,
    request: &serde_json::Value,
    method: &str,
) -> serde_json::Value {
    let id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let params = request.get("params");
    let error = |code: i64, message: String| serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});

    if method == "terminal/create" {
        let Some(command) = params
            .and_then(|p| p.get("command"))
            .and_then(|c| c.as_str())
        else {
            return error(-32602, "missing required 'command' param".to_string());
        };
        let args: Vec<String> = params
            .and_then(|p| p.get("args"))
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        // ACP's `env` is an array of `{name, value}` objects (matching
        // its use elsewhere in the schema), not a JSON object map.
        let env: HashMap<String, String> = params
            .and_then(|p| p.get("env"))
            .and_then(|e| e.as_array())
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        let name = entry.get("name")?.as_str()?.to_string();
                        let value = entry.get("value")?.as_str()?.to_string();
                        Some((name, value))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let cwd = params.and_then(|p| p.get("cwd")).and_then(|c| c.as_str());
        let output_byte_limit = params
            .and_then(|p| p.get("outputByteLimit"))
            .and_then(|l| l.as_u64())
            .map(|l| l as usize);

        return match acpx_conductor::TerminalHandle::spawn(
            command,
            &args,
            &env,
            cwd,
            output_byte_limit,
        )
        .await
        {
            Ok(handle) => {
                let terminal_id = format!("term-{}", uuid::Uuid::new_v4());
                proc.terminals.insert(terminal_id.clone(), handle);
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"terminalId": terminal_id}})
            }
            Err(err) => error(-32001, format!("terminal/create: {err}")),
        };
    }

    // Every other `terminal/*` method references an existing terminal by
    // id.
    let Some(terminal_id) = params
        .and_then(|p| p.get("terminalId"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
    else {
        return error(-32602, "missing required 'terminalId' param".to_string());
    };

    match method {
        "terminal/output" => match proc.terminals.get(&terminal_id) {
            Some(handle) => {
                let (output, truncated, exit_status) = handle.output().await;
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "output": String::from_utf8_lossy(&output),
                        "truncated": truncated,
                        "exitStatus": exit_status.map(|s| serde_json::json!({"exitCode": s.exit_code, "signal": s.signal})),
                    }
                })
            }
            None => error(-32602, format!("unknown terminalId '{terminal_id}'")),
        },
        "terminal/wait_for_exit" => match proc.terminals.get_mut(&terminal_id) {
            Some(handle) => match handle.wait_for_exit().await {
                Ok(status) => serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"exitStatus": {"exitCode": status.exit_code, "signal": status.signal}}
                }),
                Err(err) => error(-32001, format!("terminal/wait_for_exit: {err}")),
            },
            None => error(-32602, format!("unknown terminalId '{terminal_id}'")),
        },
        "terminal/kill" => match proc.terminals.get_mut(&terminal_id) {
            Some(handle) => match handle.kill().await {
                Ok(()) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                Err(err) => error(-32001, format!("terminal/kill: {err}")),
            },
            None => error(-32602, format!("unknown terminalId '{terminal_id}'")),
        },
        "terminal/release" => {
            // Per spec, the id becomes invalid for every other terminal/*
            // method after this -- dropping it from the map (which also
            // drops the `TerminalHandle`, killing the child via
            // `kill_on_drop` if it's still running) achieves exactly that.
            if proc.terminals.remove(&terminal_id).is_some() {
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}})
            } else {
                error(-32602, format!("unknown terminalId '{terminal_id}'"))
            }
        }
        other => error(-32601, format!("acpx gateway does not implement '{other}'")),
    }
}

/// **Phase 14 addition.** Context needed to route a `session/update`
/// notification live to a subscribed transport connection instead of
/// buffering it for `_acpx.updates` -- see `crate::notify`'s module doc
/// comment for the full rationale. Only constructed by the `_shared`
/// dispatch family (`dispatch_session_new_shared`/`dispatch_proxied_
/// shared`), the production path every `acpx-server` transport actually
/// uses. The plain `&mut self` dispatch path (`Router::dispatch_session_
/// new`/`Router::dispatch_proxied`, used by most of this crate's own
/// in-process tests, see e.g. `session_update_forwarding_test.rs`) has no
/// `SharedRouterHandle` available at its call sites and keeps the
/// pre-phase-14 buffer-only behavior unchanged by passing `None` -- this
/// is a deliberate scope decision (those tests assert on `_acpx.updates`
/// directly), not an oversight.
struct LiveNotifyCtx {
    router: SharedRouterHandle,
    agent_id: String,
}

/// Attempt to deliver a real `session/update` notification (`value`,
/// straight off a backend's stdout, still carrying its *backend-native*
/// `params.sessionId`) live to whichever gateway session it belongs to,
/// via `ctx`'s `NotificationHub`. Returns `true` if it was actually
/// delivered (a live subscriber was registered for the translated gateway
/// session id and the send succeeded) -- the caller must not also buffer
/// `value` into the `_acpx.updates` fallback in that case, or the same
/// client would see it twice.
///
/// Briefly re-locks `ctx.router` just to look up the backend-id ->
/// gateway-id translation (`SessionRegistry::find_by_backend`) and clone
/// the (cheaply cloneable) `NotificationHub` out -- consistent with every
/// other `_shared` dispatch function's "lock briefly for a lookup, release
/// before any actual I/O" convention in this file. The lock is held only
/// for a synchronous `HashMap` lookup, never across the backend I/O this
/// function itself doesn't perform.
async fn try_deliver_live(ctx: &LiveNotifyCtx, value: &serde_json::Value) -> bool {
    let Some(backend_session_id) = value
        .get("params")
        .and_then(|p| p.get("sessionId"))
        .and_then(|s| s.as_str())
    else {
        return false;
    };
    let (gateway_id, hub) = {
        let r = ctx.router.lock().await;
        let gateway_id = r
            .sessions
            .find_by_backend(&ctx.agent_id, backend_session_id);
        (gateway_id, r.notification_hub.clone())
    };
    let Some(gateway_id) = gateway_id else {
        return false;
    };
    let mut translated = value.clone();
    if let Some(session_id_field) = translated
        .get_mut("params")
        .and_then(|p| p.get_mut("sessionId"))
    {
        *session_id_field = serde_json::Value::String(gateway_id.0.clone());
    }
    hub.publish(&gateway_id.0, translated).await
}

/// **Phase 15.** The idle/background-reader gap phase 14 documented and
/// deliberately left open: `read_matching_response`'s read loop only ever
/// runs while one client call is in flight against a given backend, so a
/// notification a backend emits while nothing is currently in flight
/// against it sits unread in the OS pipe buffer until the next call
/// happens to drain it -- and never arrives at all if no further call is
/// ever made. One instance of this task is spawned (via
/// [`Router::spawn_idle_scavenger_if_new`]) the first time each physical
/// backend process is seen, and keeps running for that exact process
/// instance's whole lifetime.
///
/// **How it avoids racing `read_matching_response`.** Both this task and
/// every in-flight call read from the exact same `BackendProcess::reader`
/// -- one child process's stdout is a single stream, so only one reader
/// may ever be draining it at a time, or frames get corrupted/misrouted
/// between two concurrent readers. `backend.try_lock()` is the mechanism
/// that guarantees this: an in-flight call already holds this exact
/// process's own lock for its entire `read_matching_response` loop (by
/// design, see that function's doc comment), so `try_lock()` fails
/// (`Err`) for the whole time a real call owns this backend, and this
/// task simply backs off and retries later -- it never touches `reader`
/// except during the strictly-idle windows where no call holds the lock
/// at all. Conversely, while this task *does* hold the lock (briefly, one
/// non-blocking drain pass), a new call's own `backend.lock().await`
/// simply queues behind it for that same bounded moment, never anything
/// close to a whole call's real-LLM-latency duration -- this preserves
/// the "no lock held across backend I/O" discipline every other function
/// in this file follows, it just adds one more brief, bounded holder of
/// the same lock.
///
/// **What it does with what it finds.** Only a bare notification (a
/// `method`, no `id`) is actionable outside of any call context; an
/// id-bearing frame (an agent-initiated request, or a stray response with
/// no waiting caller) has no in-flight call here to answer or hand it to,
/// so it's logged and dropped rather than guessed at -- in practice this
/// should never happen, since every agent-initiated request this
/// codebase knows how to answer (`session/request_permission`, `fs/*`,
/// `terminal/*`) is only ever sent by a well-behaved backend mid an
/// already in-flight `session/prompt`, which means a real call already
/// holds this exact lock throughout, so this task would never observe
/// one in the first place; logging (not silently discarding) covers the
/// case where that assumption turns out to be wrong against some real
/// adapter. A `session/update` is delivered live via
/// [`try_deliver_live`], the exact same path/translation/hub a call
/// in-flight would have used -- if that succeeds, this closes the phase
/// 14 gap precisely: an update that arrived between prompt turns now
/// reaches a subscribed stdio/WS connection instead of waiting, possibly
/// forever, for the next call to that backend. If no live subscriber is
/// registered (or the notification isn't `session/update`) there is
/// still nothing to buffer it into -- no in-flight call's `_acpx.updates`
/// exists right now -- so it's logged and discarded, same as it always
/// effectively was pre-phase-15 (silently sitting unread forever), except
/// now it's observed rather than invisible. Extending idle notifications
/// to also feed the *next* call's `_acpx.updates` bundle (for `POST
/// /rpc`-style clients with no live connection to push to at all) is left
/// out of scope on purpose -- `POST /rpc` was already excluded from live
/// delivery entirely in phase 14 for the same "no persistent connection
/// to push to" reason, and this phase's gap statement is specifically
/// about the two transports capable of a live push in the first place.
async fn backend_idle_scavenger(
    backend: acpx_conductor::supervisor::SharedBackendProcess,
    ctx: LiveNotifyCtx,
) {
    loop {
        tokio::time::sleep(Duration::from_millis(75)).await;
        let Ok(mut proc) = backend.try_lock() else {
            // A real call owns this backend right now -- its own
            // `read_matching_response` loop is already draining
            // `reader`, so there is nothing for this tick to do.
            continue;
        };
        if proc.has_exited() {
            // This physical process instance is gone for good (a
            // respawn, if any, is a brand new `SharedBackendProcess`
            // with its own fresh scavenger, see `Router::
            // spawn_idle_scavenger_if_new`'s doc comment) -- nothing
            // left to scavenge, stop the task rather than spin forever.
            return;
        }
        // Drain every frame already sitting in the OS pipe buffer, but
        // never block waiting for one that hasn't arrived yet -- a
        // zero-duration `timeout` around one `read_value` call is this
        // function's "try a non-blocking read" idiom: data already
        // available resolves on the very first poll, exactly like a real
        // read would; anything not yet available times out immediately
        // instead of parking this task (and the process lock it's
        // holding) waiting for it.
        loop {
            let attempt =
                tokio::time::timeout(Duration::from_millis(0), proc.reader.read_value()).await;
            let value = match attempt {
                Ok(Ok(value)) => value,
                Ok(Err(err)) => {
                    tracing::warn!(
                        agent_id = %ctx.agent_id,
                        %err,
                        "acpx idle scavenger's backend read errored; stopping this backend's scavenger"
                    );
                    return;
                }
                Err(_) => break, // nothing ready right now -- hand the lock back
            };
            if value.get("id").is_some() {
                tracing::warn!(
                    agent_id = %ctx.agent_id,
                    ?value,
                    "acpx idle scavenger saw an id-bearing frame with no in-flight caller; ignoring"
                );
                continue;
            }
            if value.get("method").and_then(|m| m.as_str()) == Some("session/update")
                && try_deliver_live(&ctx, &value).await
            {
                continue;
            }
            tracing::debug!(
                agent_id = %ctx.agent_id,
                ?value,
                "acpx idle scavenger drained a notification with no live subscriber to deliver it to; discarding"
            );
        }
    }
}

async fn read_matching_response(
    backend: &mut acpx_conductor::BackendProcess,
    id: &serde_json::Value,
    policy: BackendCallPolicy,
    live: Option<&LiveNotifyCtx>,
) -> Result<
    (
        serde_json::Value,
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
    ),
    RouterError,
> {
    let mut notifications = Vec::new();
    let mut agent_requests = Vec::new();
    loop {
        let value = backend.reader.read_value().await?;
        if value.get("id") == Some(id) {
            return Ok((value, notifications, agent_requests));
        }
        // An agent-initiated *request* (has both its own `id` and a
        // `method`) is not a notification -- pre-fix, this loop treated
        // it as one, pushing it into `notifications` and never replying,
        // which left the backend deadlocked forever waiting for an
        // answer that would never come (verified as a real hang, not a
        // hypothetical, against `session/request_permission`: every real
        // adapter that asks permission mid-turn blocks its own response
        // to the *outer* call on getting one). `session/request_permission`
        // is the only such method acpx knows how to answer today (see
        // `build_permission_reply`); anything else gets a proper JSON-RPC
        // method-not-found error instead of silence, so a backend that
        // sends some other agent-initiated request acpx doesn't yet
        // support still gets *a* reply and can decide how to proceed
        // (e.g. treat it as declined) rather than hanging indefinitely.
        if let (Some(_), Some(method)) = (
            value.get("id"),
            value.get("method").and_then(|m| m.as_str()),
        ) {
            let reply = if method == "session/request_permission" {
                build_permission_reply(&value, policy.permission_policy)
            } else if (method == "fs/read_text_file" || method == "fs/write_text_file")
                && policy.allow_fs_access
            {
                handle_fs_request(&value, method).await
            } else if method == "fs/read_text_file" || method == "fs/write_text_file" {
                // Capability wasn't enabled for this profile -- declared
                // `false` in `initialize`, so a well-behaved backend
                // shouldn't be asking at all, but reply with a clear
                // "not enabled" error (not a plain method-not-found) if
                // one does anyway, distinguishing "acpx doesn't have this
                // handler" from "this profile turned it off".
                let req_id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32601,
                        "message": format!("'{method}' is disabled for this profile (Profile::allow_fs_access is false)"),
                    }
                })
            } else if (method == "terminal/create"
                || method == "terminal/output"
                || method == "terminal/wait_for_exit"
                || method == "terminal/kill"
                || method == "terminal/release")
                && policy.allow_terminal_access
            {
                handle_terminal_request(backend, &value, method).await
            } else if method == "terminal/create"
                || method == "terminal/output"
                || method == "terminal/wait_for_exit"
                || method == "terminal/kill"
                || method == "terminal/release"
            {
                // Same "disabled, not unsupported" distinction as the
                // `fs/*` arm above, gated on `Profile::allow_terminal_access`.
                let req_id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32601,
                        "message": format!("'{method}' is disabled for this profile (Profile::allow_terminal_access is false)"),
                    }
                })
            } else {
                let req_id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32601,
                        "message": format!("acpx gateway does not support agent-initiated method '{method}'"),
                    }
                })
            };
            backend.writer.lock().await.write_value(&reply).await?;
            agent_requests.push(serde_json::json!({"request": value, "reply": reply}));
            continue;
        }
        // **Phase 14.** A real notification (`method`, no `id`) -- try
        // live delivery first when a subscribed transport connection is
        // known (`live.is_some()`) and this is the one notification type
        // that's actually session-scoped and worth streaming live,
        // `session/update`. Anything not delivered live (no `live` ctx at
        // all, e.g. the plain `&mut self` dispatch path; no live
        // subscriber currently registered for this session, e.g. an
        // HTTP-only client; or a notification method other than
        // `session/update`) falls through to the pre-existing buffering
        // behavior unchanged, so `_acpx.updates` keeps working exactly as
        // before for every case this phase doesn't newly handle.
        if let Some(ctx) = live {
            if value.get("method").and_then(|m| m.as_str()) == Some("session/update")
                && try_deliver_live(ctx, &value).await
            {
                continue;
            }
        }
        notifications.push(value);
    }
}

/// Fold `notifications` and `agent_requests` (both as collected by
/// [`read_matching_response`]) into `response`'s `_acpx.updates`/
/// `_acpx.agentRequests` arrays, if there are any of either. `agentRequests`
/// is new: every `{request, reply}` pair `read_matching_response` had to
/// answer on the client's behalf mid-call (a `session/request_permission`
/// auto-decision per `crate::profile::PermissionPolicy`, or a method-not-
/// found error for any other agent-initiated request acpx doesn't yet
/// support) -- surfaced so a client can see what was decided without
/// acpx silently hiding it. No-op (response left byte-for-byte untouched)
/// when both are empty, so a stand-in backend that never emits either
/// (every synthetic test double in this workspace, until one is written
/// specifically to exercise this) produces identical response shapes to
/// before this fix -- verified by every pre-existing test in this
/// workspace continuing to pass unmodified.
fn attach_updates(
    mut response: serde_json::Value,
    notifications: Vec<serde_json::Value>,
    agent_requests: Vec<serde_json::Value>,
) -> serde_json::Value {
    if notifications.is_empty() && agent_requests.is_empty() {
        return response;
    }
    if let Some(obj) = response.as_object_mut() {
        let mut extras = serde_json::Map::new();
        if !notifications.is_empty() {
            extras.insert("updates".to_string(), serde_json::json!(notifications));
        }
        if !agent_requests.is_empty() {
            extras.insert(
                "agentRequests".to_string(),
                serde_json::json!(agent_requests),
            );
        }
        obj.insert("_acpx".to_string(), serde_json::Value::Object(extras));
    }
    response
}

/// `session/new`-specific twin of [`attach_updates`]: same additive,
/// namespaced `_acpx` merge, plus the backend's real `agentCapabilities`
/// (as captured by `ensure_backend_initialized`, see that function's doc
/// comment for why this exists) under `_acpx.agentCapabilities`. Kept as
/// a separate function rather than adding a third parameter to
/// `attach_updates` -- every other proxied method (`session/prompt` etc.)
/// has no use for a backend's `initialize`-time capabilities on every
/// single call, only the one call that starts the session at all.
/// No-op on both fronts (response left byte-for-byte untouched) when
/// there are no notifications and no captured capabilities, so every
/// pre-existing synthetic-backend test -- whose stand-in scripts don't
/// answer `initialize` with anything resembling a real `agentCapabilities`
/// object -- keeps producing identical response shapes.
fn attach_session_new_extras(
    mut response: serde_json::Value,
    notifications: Vec<serde_json::Value>,
    agent_requests: Vec<serde_json::Value>,
    agent_capabilities: Option<serde_json::Value>,
) -> serde_json::Value {
    if notifications.is_empty() && agent_requests.is_empty() && agent_capabilities.is_none() {
        return response;
    }
    if let Some(obj) = response.as_object_mut() {
        let mut extras = serde_json::Map::new();
        if !notifications.is_empty() {
            extras.insert("updates".to_string(), serde_json::json!(notifications));
        }
        if !agent_requests.is_empty() {
            extras.insert(
                "agentRequests".to_string(),
                serde_json::json!(agent_requests),
            );
        }
        if let Some(capabilities) = agent_capabilities {
            extras.insert("agentCapabilities".to_string(), capabilities);
        }
        obj.insert("_acpx".to_string(), serde_json::Value::Object(extras));
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
        // **Phase 7:** `session/cancel` needs `dispatch_session_cancel_shared`
        // specifically, not the generic `dispatch_proxied_shared` --
        // see `Router::dispatch_session_cancel`'s doc comment for why
        // (id-optional notification shape, no blocking wait for a reply
        // that will never come, and -- the part that actually makes
        // cancellation *work*, not just avoid erroring -- a write path
        // independent of the per-process lock a concurrent
        // `session/prompt` against the same backend may be holding for
        // its entire duration).
        MethodClass::Proxied if method == "session/cancel" => {
            dispatch_session_cancel_shared(router, request).await
        }
        MethodClass::Proxied => dispatch_proxied_shared(router, request).await,
        // **Phase 13.** Mirrors `dispatch_native`'s `"session/list"`
        // branching (see `session_list_selector`'s doc comment) but only
        // when a selector is actually present -- an unqualified
        // `session/list` stays on the generic `GatewayNative` path just
        // below, cheap/local exactly as before. When a selector *is*
        // present this is genuinely a backend round trip (a real
        // `Proxied`-shaped call under the hood), so it gets its own
        // lock-briefly-then-release `_shared` variant like every other
        // backend-talking path in this function -- routing it through
        // the generic `router.lock().await.dispatch(request).await`
        // arm instead would hold the *entire* router lock for the whole
        // backend round trip, blocking every other concurrent client
        // (including ones talking to unrelated backends) for no reason,
        // exactly the regression this function's own doc comment above
        // exists to prevent.
        MethodClass::GatewayNative
            if method == "session/list"
                && request
                    .get("params")
                    .and_then(session_list_selector)
                    .is_some() =>
        {
            dispatch_session_list_real_shared(router, request).await
        }
        MethodClass::GatewayNative | MethodClass::Unknown => {
            router.lock().await.dispatch(request).await
        }
    }
}

/// [`dispatch_shared`]'s `session/cancel` path -- mirrors
/// `Router::dispatch_session_cancel` exactly (see that method's doc
/// comment for the full rationale) but restructured the same way every
/// other `_shared` function in this file is: resolve session/agent state
/// under `router`'s own brief lock, then release it before touching a
/// backend at all. The release matters even more here than for the
/// generic proxied path: `Supervisor::cancel_writer`'s entire purpose is
/// letting this write proceed without contending with a concurrent
/// `session/prompt`'s per-process lock, so holding `router`'s own lock
/// any longer than strictly necessary to look up the writer handle would
/// undermine that (a `session/prompt` against a *different* agent, or a
/// `session/new` for a brand new one, would otherwise queue up behind
/// this cancel call for no reason).
async fn dispatch_session_cancel_shared(
    router: &SharedRouterHandle,
    request: serde_json::Value,
) -> Result<serde_json::Value, RouterError> {
    let client_id = request
        .get("id")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let gateway_session_id = request
        .get("params")
        .and_then(|p| p.get("sessionId"))
        .and_then(|s| s.as_str())
        .ok_or(RouterError::MissingSessionId)?
        .to_string();

    let (backend_session_id, persistence, cancel_writer) = {
        let r = router.lock().await;
        let entry = r
            .sessions
            .resolve(&acpx_proto::session::GatewaySessionId(
                gateway_session_id.clone(),
            ))
            .ok_or_else(|| RouterError::UnknownSession(gateway_session_id.clone()))?;
        let backend_session_id = entry.backend_session_id.0.clone();
        let cancel_writer = r.supervisor.cancel_writer(&entry.agent_id);
        (backend_session_id, r.persistence.clone(), cancel_writer)
    };

    let notification = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": backend_session_id }
    });
    spawn_transcript_fn(
        persistence,
        gateway_session_id,
        Direction::ClientToAgent,
        notification.clone(),
    );

    // Same "nothing running, nothing to cancel" no-op as
    // `Router::dispatch_session_cancel` -- see that method's comment.
    if let Some(writer) = cancel_writer {
        writer.lock().await.write_value(&notification).await?;
    }

    Ok(serde_json::json!({ "jsonrpc": "2.0", "id": client_id, "result": {} }))
}

/// [`dispatch_shared`]'s real, per-backend `session/list` path -- see
/// `Router::dispatch_session_list_real`'s doc comment for the full
/// rationale (this mirrors it exactly: `_acpx` selector resolution,
/// forward, backend-id -> gateway-id translation) restructured the same
/// way every other `_shared` function in this file is, to release
/// `router`'s lock before the backend round trip rather than holding it
/// for the call's entire duration.
async fn dispatch_session_list_real_shared(
    router: &SharedRouterHandle,
    request: serde_json::Value,
) -> Result<serde_json::Value, RouterError> {
    let id = request.get("id").cloned().ok_or(RouterError::MissingId)?;
    let mut params = request
        .get("params")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let selector = session_list_selector(&params).expect(
        "dispatch_shared only routes to this function when session_list_selector(params) is Some",
    );
    if let Some(obj) = params.as_object_mut() {
        obj.remove("_acpx");
    }

    let (agent_id, profile_name, backend, call_policy) = {
        let mut r = router.lock().await;
        let (agent_id, profile) = match selector {
            SessionListSelector::Profile(name) => {
                let (key, profile) = r.resolve_profile(&name).await?;
                (key, Some(profile))
            }
            SessionListSelector::AgentId(explicit_id) => (explicit_id, None),
        };
        let profile_name = profile.as_ref().map(|p| p.name.clone());
        let backend = r.supervisor.ensure_running(&agent_id).await?;
        r.spawn_idle_scavenger_if_new(router, &agent_id, &backend);
        let call_policy = BackendCallPolicy::from_profile(profile.as_ref());
        (agent_id, profile_name, backend, call_policy)
    };

    let outbound = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/list",
        "params": params,
    });

    let response = {
        let mut proc = backend.lock().await;
        ensure_backend_initialized(&mut proc, call_policy.clone()).await?;
        proc.writer.lock().await.write_value(&outbound).await?;
        let (response, _notifications, _agent_requests) =
            read_matching_response(&mut proc, &id, call_policy, None).await?;
        response
    };

    if let Some(error) = response.get("error") {
        return Err(RouterError::BackendSessionListError(error.clone()));
    }

    let mut result = response
        .get("result")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({ "sessions": [] }));
    if let Some(sessions) = result.get_mut("sessions").and_then(|s| s.as_array_mut()) {
        let mut r = router.lock().await;
        for session in sessions.iter_mut() {
            let Some(backend_sid) = session
                .get("sessionId")
                .and_then(|s| s.as_str())
                .map(str::to_string)
            else {
                continue;
            };
            let session_cwd = session
                .get("cwd")
                .and_then(|c| c.as_str())
                .map(str::to_string);
            let gateway_id = r.translate_or_register_backend_session(
                &agent_id,
                &backend_sid,
                profile_name.clone(),
                session_cwd,
            );
            session["sessionId"] = serde_json::Value::String(gateway_id.clone());
            spawn_session_persistence_fn(
                r.persistence.clone(),
                gateway_id,
                agent_id.clone(),
                backend_sid,
                profile_name.clone(),
                outbound.clone(),
                response.clone(),
            );
        }
    }

    Ok(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }))
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

    let (backend, persistence, call_policy, agent_id) = {
        let mut r = router.lock().await;
        let entry = match r.sessions.resolve(&acpx_proto::session::GatewaySessionId(
            gateway_session_id.clone(),
        )) {
            Some(entry) => entry.clone(),
            None => r.rehydrate_session(&method, &gateway_session_id).await?,
        };
        let agent_id = entry.agent_id.clone();
        let backend_session_id = entry.backend_session_id.0.clone();
        let profile_name = entry.profile_name.clone();
        if let Some(params) = request.get_mut("params") {
            params["sessionId"] = serde_json::Value::String(backend_session_id);
        }
        let backend = r.supervisor.ensure_running(&agent_id).await?;
        r.spawn_idle_scavenger_if_new(router, &agent_id, &backend);
        let call_policy = BackendCallPolicy::from_profile(
            profile_name
                .as_deref()
                .and_then(|name| r.profiles.get(name)),
        );
        (backend, r.persistence.clone(), call_policy, agent_id)
    };

    spawn_transcript_fn(
        persistence.clone(),
        gateway_session_id.clone(),
        Direction::ClientToAgent,
        request.clone(),
    );

    let response = {
        let mut proc = backend.lock().await;
        ensure_backend_initialized(&mut proc, call_policy.clone()).await?;
        proc.writer.lock().await.write_value(&request).await?;
        let live = LiveNotifyCtx {
            router: std::sync::Arc::clone(router),
            agent_id,
        };
        let (response, notifications, agent_requests) =
            read_matching_response(&mut proc, &id, call_policy, Some(&live)).await?;
        attach_updates(response, notifications, agent_requests)
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

    let (agent_id, profile, backend, persistence, cwd) = {
        let mut r = router.lock().await;
        let params = request
            .get_mut("params")
            .ok_or(RouterError::MissingParams)?;
        let cwd = params
            .get("cwd")
            .and_then(|c| c.as_str())
            .map(str::to_string);
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
        r.spawn_idle_scavenger_if_new(router, &agent_id, &backend);
        (agent_id, profile, backend, r.persistence.clone(), cwd)
    };

    let mut response = {
        let mut proc = backend.lock().await;
        let call_policy = BackendCallPolicy::from_profile(profile.as_ref());
        ensure_backend_initialized(&mut proc, call_policy.clone()).await?;
        proc.writer.lock().await.write_value(&request).await?;
        // No `LiveNotifyCtx` here, deliberately: this exact call is what
        // *creates* the gateway session (`self.sessions.register` below,
        // after this block returns) -- until that registration happens, no
        // gateway session id exists yet for `try_deliver_live`'s
        // `find_by_backend` lookup to ever find, and no transport
        // connection could possibly have subscribed to it yet either (a
        // connection only learns the gateway session id from *this*
        // call's own response). Passing a live ctx here would be dead
        // code that always falls back to buffering -- `session/prompt`/
        // `session/resume`/`session/load` (`dispatch_proxied_shared`,
        // which *does* pass one) are where live delivery actually
        // matters, since those always target an already-registered
        // session.
        let (response, notifications, agent_requests) =
            read_matching_response(&mut proc, &id, call_policy, None).await?;
        attach_session_new_extras(
            response,
            notifications,
            agent_requests,
            proc.agent_capabilities.clone(),
        )
    };

    let backend_session_id = extract_backend_session_id(&response)?;

    let (gateway_session_id_str, persist_args) = {
        let mut r = router.lock().await;
        let gateway_id = r.sessions.register(
            agent_id,
            BackendSessionId(backend_session_id),
            profile.as_ref().map(|p| p.name.clone()),
            cwd,
        );
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

    #[tokio::test]
    async fn handle_fs_request_windows_read_by_line_and_limit() {
        // Unit-level coverage for the `line`/`limit` windowing math itself
        // (1-indexed start, max-lines cap) -- `fs_request_test.rs` covers
        // the no-params "whole file" path end to end through `Router`;
        // this covers the windowing arithmetic directly since threading a
        // `line`/`limit`-carrying request through a shell stand-in backend
        // would be awkward to express.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("windowed.txt");
        std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").unwrap();

        let request = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "fs/read_text_file",
            "params": {"path": path.to_str().unwrap(), "line": 2, "limit": 2}
        });
        let reply = handle_fs_request(&request, "fs/read_text_file").await;
        // 1-indexed `line: 2` starts at "two"; `limit: 2` caps it there.
        assert_eq!(reply["result"]["content"], serde_json::json!("two\nthree"));
    }

    #[tokio::test]
    async fn handle_fs_request_read_of_missing_file_is_a_clear_error_not_a_panic() {
        let request = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "fs/read_text_file",
            "params": {"path": "/definitely/does/not/exist.txt"}
        });
        let reply = handle_fs_request(&request, "fs/read_text_file").await;
        assert!(reply.get("error").is_some());
        assert_eq!(
            reply["error"]["data"]["path"],
            serde_json::json!("/definitely/does/not/exist.txt")
        );
    }

    #[test]
    fn classifies_mcp_server_methods_as_gateway_native() {
        assert_eq!(classify("mcp_servers/create"), MethodClass::GatewayNative);
        assert_eq!(classify("mcp_servers/list"), MethodClass::GatewayNative);
        assert_eq!(classify("mcp_servers/update"), MethodClass::GatewayNative);
        assert_eq!(classify("mcp_servers/delete"), MethodClass::GatewayNative);
    }

    /// **Phase 9.** `session/delete` and `logout` were entirely
    /// unclassified (fell through to `Unknown`) before this phase, same
    /// category of gap as phase 6's pre-fix `initialize`/`authenticate`.
    #[test]
    fn classifies_phase_9_stable_methods() {
        assert_eq!(classify("session/delete"), MethodClass::Proxied);
        assert_eq!(classify("logout"), MethodClass::GatewayNative);
    }
}
