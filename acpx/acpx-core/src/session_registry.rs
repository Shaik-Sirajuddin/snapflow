//! Gateway session id -> (agent, backend session id) mapping. Phase 2
//! step 8 in the phased plan; Phase 1's single-agent spike doesn't need
//! this (see `acpx-server`'s Phase 1 passthrough), but it's cheap to stand
//! up now since it has no dependency on multi-agent routing.

use acpx_proto::session::GatewaySessionId;
use std::collections::HashMap;

/// Opaque backend-native session id, as returned by whatever backend agent
/// answered `session/new`. Kept distinct from `GatewaySessionId` so the two
/// can never be swapped by accident at a call site.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BackendSessionId(pub String);

#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub agent_id: String,
    pub backend_session_id: BackendSessionId,
    /// Which profile (if any) `session/new` resolved this session
    /// through -- `None` for native/unmanaged mode. Threaded through so
    /// later proxied calls on this same session (`session/prompt` etc.)
    /// can look the profile back up for its
    /// `crate::profile::PermissionPolicy` when a backend sends a
    /// `session/request_permission` request mid-call; see
    /// `crate::router::read_matching_response`.
    pub profile_name: Option<String>,
    /// The session's working directory, if known. Populated from the
    /// client's own `session/new` request (`params.cwd`) or from a real
    /// backend's `session/list` response (`SessionInfo.cwd`) when a
    /// session is discovered that way -- see `Router::
    /// translate_or_register_backend_session`/`dispatch_session_list_real`.
    /// **Phase 13 addition**, closes part of a real spec gap: the real
    /// `SessionInfo` schema marks `cwd` as *required*, but nothing in
    /// this registry tracked it before this phase, so acpx's own
    /// gateway-scoped `session/list` aggregate could never honestly
    /// include it. `None` for sessions rehydrated from a persisted
    /// `SessionRecord` predating this field (the sqlite `sessions` table
    /// itself doesn't carry `cwd` yet -- a known, tracked follow-up, not
    /// silently dropped) or from any other path that never learned it.
    pub cwd: Option<String>,
}

#[derive(Debug, Default)]
pub struct SessionRegistry {
    sessions: HashMap<String, SessionEntry>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a newly-created session, minting a fresh gateway session id.
    pub fn register(
        &mut self,
        agent_id: impl Into<String>,
        backend_session_id: BackendSessionId,
        profile_name: Option<String>,
        cwd: Option<String>,
    ) -> GatewaySessionId {
        let gateway_id = GatewaySessionId(uuid::Uuid::new_v4().to_string());
        self.sessions.insert(
            gateway_id.0.clone(),
            SessionEntry {
                agent_id: agent_id.into(),
                backend_session_id,
                profile_name,
                cwd,
            },
        );
        gateway_id
    }

    pub fn resolve(&self, gateway_id: &GatewaySessionId) -> Option<&SessionEntry> {
        self.sessions.get(&gateway_id.0)
    }

    /// Re-insert a session under an *already-known* gateway id, rather
    /// than minting a fresh one via [`Self::register`]. Phase 8 addition:
    /// backs `session/load`/`session/resume`'s rehydration path -- a
    /// gateway process restart clears this in-memory map entirely, but a
    /// spec-compliant client is fully entitled to call `session/load`
    /// with a gateway session id it was handed by a *previous* acpx
    /// process lifetime (that's the entire point of `session/load`
    /// existing as a distinct method from `session/new`). The caller is
    /// responsible for sourcing `entry` from durable storage (see
    /// `crate::persistence::PersistenceStore::get_session`) before
    /// calling this -- this method itself does no I/O, it only accepts
    /// whatever the caller already resolved and makes it resolvable
    /// in-memory again under the same id.
    pub fn insert(&mut self, gateway_id: GatewaySessionId, entry: SessionEntry) {
        self.sessions.insert(gateway_id.0, entry);
    }

    pub fn remove(&mut self, gateway_id: &GatewaySessionId) -> Option<SessionEntry> {
        self.sessions.remove(&gateway_id.0)
    }

    /// Aggregated `session/list` -- all live sessions across every backend.
    pub fn list(&self) -> impl Iterator<Item = (&String, &SessionEntry)> {
        self.sessions.iter()
    }

    /// Reverse lookup: does a gateway session id already exist for this
    /// exact `(agent_id, backend_session_id)` pair? **Phase 13 addition.**
    /// Backs the real, per-backend `session/list` path's backend-id ->
    /// gateway-id translation (`Router::
    /// translate_or_register_backend_session`): a real backend's
    /// `session/list` response only ever carries its own native session
    /// ids, per the ACP schema, but every other proxied method in this
    /// router (`session/load`, `session/prompt`, ...) only ever accepts
    /// a *gateway* id. This lets that translation reuse an already-known
    /// gateway id (e.g. a session acpx itself opened earlier in this
    /// process's lifetime) instead of minting a duplicate one every time
    /// the same backend session is listed again.
    pub fn find_by_backend(
        &self,
        agent_id: &str,
        backend_session_id: &str,
    ) -> Option<GatewaySessionId> {
        self.sessions.iter().find_map(|(gid, entry)| {
            if entry.agent_id == agent_id && entry.backend_session_id.0 == backend_session_id {
                Some(GatewaySessionId(gid.clone()))
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_resolve_round_trips() {
        let mut reg = SessionRegistry::new();
        let gid = reg.register(
            "codex-acp",
            BackendSessionId("backend-1".to_string()),
            None,
            None,
        );
        let entry = reg.resolve(&gid).expect("just registered");
        assert_eq!(entry.agent_id, "codex-acp");
        assert_eq!(entry.backend_session_id.0, "backend-1");
    }

    #[test]
    fn remove_forgets_the_session() {
        let mut reg = SessionRegistry::new();
        let gid = reg.register(
            "codex-acp",
            BackendSessionId("backend-1".to_string()),
            None,
            None,
        );
        assert!(reg.remove(&gid).is_some());
        assert!(reg.resolve(&gid).is_none());
    }

    #[test]
    fn find_by_backend_locates_an_already_registered_session() {
        let mut reg = SessionRegistry::new();
        let gid = reg.register(
            "codex-acp",
            BackendSessionId("backend-1".to_string()),
            None,
            Some("/tmp".to_string()),
        );
        assert_eq!(reg.find_by_backend("codex-acp", "backend-1"), Some(gid));
        assert_eq!(reg.find_by_backend("codex-acp", "backend-2"), None);
        assert_eq!(reg.find_by_backend("other-agent", "backend-1"), None);
    }
}
