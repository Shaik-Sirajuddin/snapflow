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
    ) -> GatewaySessionId {
        let gateway_id = GatewaySessionId(uuid::Uuid::new_v4().to_string());
        self.sessions.insert(
            gateway_id.0.clone(),
            SessionEntry {
                agent_id: agent_id.into(),
                backend_session_id,
                profile_name,
            },
        );
        gateway_id
    }

    pub fn resolve(&self, gateway_id: &GatewaySessionId) -> Option<&SessionEntry> {
        self.sessions.get(&gateway_id.0)
    }

    pub fn remove(&mut self, gateway_id: &GatewaySessionId) -> Option<SessionEntry> {
        self.sessions.remove(&gateway_id.0)
    }

    /// Aggregated `session/list` -- all live sessions across every backend.
    pub fn list(&self) -> impl Iterator<Item = (&String, &SessionEntry)> {
        self.sessions.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_resolve_round_trips() {
        let mut reg = SessionRegistry::new();
        let gid = reg.register("codex-acp", BackendSessionId("backend-1".to_string()), None);
        let entry = reg.resolve(&gid).expect("just registered");
        assert_eq!(entry.agent_id, "codex-acp");
        assert_eq!(entry.backend_session_id.0, "backend-1");
    }

    #[test]
    fn remove_forgets_the_session() {
        let mut reg = SessionRegistry::new();
        let gid = reg.register("codex-acp", BackendSessionId("backend-1".to_string()), None);
        assert!(reg.remove(&gid).is_some());
        assert!(reg.resolve(&gid).is_none());
    }
}
