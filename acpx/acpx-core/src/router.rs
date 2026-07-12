//! Method classification (gateway-native vs. proxied vs. hybrid) per
//! `02-architecture.md`'s classification table. Phase 1 only needs
//! classification for the single-agent passthrough set; profile
//! resolution, MCP-server merge, and gateway-native handlers land in
//! Phase 2/3.

use crate::persistence::{Direction, PersistenceStore};
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
        | "session/set_mode" | "session/cancel" => MethodClass::Proxied,
        "agents/list" | "agents/install" | "agents/status" | "session/list" => {
            MethodClass::GatewayNative
        }
        "profiles/create" | "profiles/list" | "profiles/update" | "profiles/delete" => {
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
/// **Known Phase 2 gap** (tracked in `05-open-risks.md`'s
/// "Reverse-direction (agent-initiated) messages" item): agent-initiated
/// messages that arrive on a backend's stdout without a matching request id
/// (e.g. `session/update` notifications) are currently logged and dropped
/// rather than routed back to the owning client connection -- there is no
/// reverse-direction wiring yet, since Phase 2 hasn't connected a
/// multi-client transport to this `Router` (that's `acpx-server`'s HTTP/WS
/// work, step 11, still pending). `acpx-server`'s Phase 1 stdio spike also
/// still bypasses this `Router` entirely for the same reason -- it proxies
/// one client to one backend directly, so it never needed
/// request/response correlation across concurrent sessions.
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

    /// Fire-and-forget one transcript append, if persistence is attached.
    /// Never awaited by the caller -- spawned onto the runtime so a slow
    /// sqlite write can't add latency to the client-visible request path.
    fn spawn_transcript(
        &self,
        gateway_session_id: impl Into<String>,
        direction: Direction,
        payload: serde_json::Value,
    ) {
        let Some(store) = self.persistence.clone() else {
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
        client_request: serde_json::Value,
        agent_response: serde_json::Value,
    ) {
        let Some(store) = self.persistence.clone() else {
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
                    None,
                    now_rfc3339(),
                )
                .await
            {
                tracing::warn!(%err, "failed to persist session metadata");
                // Don't attempt the transcript inserts -- without a
                // `sessions` row they'd only fail the same FK check.
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
        // selects managed mode (Phase 3 resolves profile -> agent/provider);
        // until then it only picks which registered agent id to use, same
        // as native mode's `default_agent_id` fallback. Either way `_acpx`
        // is stripped before forwarding -- session/new stays a raw-ACP
        // drop-in for a client that never set it.
        let agent_id = params
            .get("_acpx")
            .and_then(|ext| ext.get("profile"))
            .and_then(|p| p.as_str())
            .unwrap_or(&self.default_agent_id)
            .to_string();
        if let Some(obj) = params.as_object_mut() {
            obj.remove("_acpx");
        }

        let backend = self.supervisor.ensure_running(&agent_id).await?;
        backend.writer.write_value(&request).await?;
        let mut response = read_matching_response(backend, &id).await?;

        let backend_session_id = response
            .get("result")
            .and_then(|r| r.get("sessionId"))
            .and_then(|s| s.as_str())
            .ok_or(RouterError::MissingBackendSessionId)?
            .to_string();
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
                request,
                response.clone(),
            );
        }
        Ok(response)
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
        backend.writer.write_value(&request).await?;
        let response = read_matching_response(backend, &id).await?;
        self.spawn_transcript(
            gateway_session_id.clone(),
            Direction::AgentToClient,
            response.clone(),
        );
        if method == "session/close" {
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
            "profiles/create" | "profiles/list" | "profiles/update" | "profiles/delete" => {
                return Err(RouterError::NotImplemented(
                    "profile CRUD (Phase 3 step 14)",
                ))
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
/// sent. Anything else (an agent-initiated notification/request with no
/// matching id, most notably `session/update`) is logged and dropped --
/// see the `Router` doc comment's "Known Phase 2 gap" note.
async fn read_matching_response(
    backend: &mut acpx_conductor::BackendProcess,
    id: &serde_json::Value,
) -> Result<serde_json::Value, RouterError> {
    loop {
        let value = backend.reader.read_value().await?;
        if value.get("id") == Some(id) {
            return Ok(value);
        }
        tracing::warn!(
            ?value,
            "dropping unmatched backend message (no reverse-direction routing yet, see 05-open-risks.md)"
        );
    }
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
}
