//! Transport-agnostic virtual sessions used by an ACP compatibility bridge.
//!
//! A bridge session is deliberately separate from [`crate::SessionRegistry`]:
//! it exists before an adapter session has been created, so it must never
//! invent or store a backend session id. The first prompt claims binding
//! ownership atomically; later prompts can observe that binding is in flight.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use acpx_proto::session::NewSessionParams;

use crate::TenantId;

/// Opaque id for a virtual bridge session.
///
/// IDs are scoped by [`TenantId`] in [`BridgeSessionStore`]'s internal map.
/// The UUID makes accidental reuse unlikely, while the nested map makes
/// tenant isolation a structural invariant rather than a UUID assumption.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BridgeSessionId(pub String);

/// The lazy-binding lifecycle of a bridge session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeSessionState {
    /// No adapter session has been requested yet.
    Unbound,
    /// One prompt currently owns the work of creating the adapter session.
    Binding,
    /// Binding completed successfully.
    Bound,
    /// Binding completed unsuccessfully.
    Failed,
}

/// A virtual session snapshot returned by [`BridgeSessionStore`] operations.
///
/// `original_new_session_params` is retained verbatim so a later binding
/// implementation can create its adapter session from the original client
/// request. `cwd` is duplicated as an explicit convenience field for callers
/// that need it without inspecting the protocol payload.
#[derive(Debug, Clone)]
pub struct BridgeSession {
    pub id: BridgeSessionId,
    pub original_new_session_params: NewSessionParams,
    pub cwd: String,
    pub selected_public_model_alias: Option<String>,
    /// Adapter-native configuration choices selected before lazy binding,
    /// keyed by the native ACP `configId` (for example `permissionMode`).
    pub selected_adapter_config_options: HashMap<String, String>,
    /// ACPX-native gateway id created after lazy binding. This remains
    /// internal: bridge clients keep using [`Self::id`] for the lifetime of
    /// the virtual session.
    pub bound_gateway_session_id: Option<String>,
    pub state: BridgeSessionState,
}

/// The result of atomically attempting to begin lazy binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingClaim {
    /// This caller moved the session from `Unbound` to `Binding`.
    Owner,
    /// Another caller already owns binding work.
    Binding,
    /// Binding has already completed successfully.
    Bound,
    /// Binding has already failed.
    Failed,
}

/// Errors returned when a tenant-scoped bridge-session operation cannot run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BridgeSessionError {
    #[error("bridge session {session_id} was not found for tenant {tenant_id}")]
    NotFound {
        tenant_id: String,
        session_id: String,
    },
    #[error("cannot {operation} bridge session {session_id} while it is {state:?}")]
    InvalidState {
        operation: &'static str,
        session_id: String,
        state: BridgeSessionState,
    },
}

/// Concurrent store of virtual bridge sessions.
///
/// Each mutating operation holds one mutex across lookup and state update, so
/// lazy-binding ownership cannot be claimed by more than one concurrent
/// prompt. Returned sessions are snapshots, never references into the store.
#[derive(Debug, Clone, Default)]
pub struct BridgeSessionStore {
    sessions: Arc<Mutex<HashMap<TenantId, HashMap<BridgeSessionId, BridgeSession>>>>,
}

impl BridgeSessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an unbound virtual session under a new tenant-scoped id.
    pub fn register(
        &self,
        tenant_id: &TenantId,
        original_new_session_params: NewSessionParams,
    ) -> BridgeSessionId {
        let session_id = BridgeSessionId(uuid::Uuid::new_v4().to_string());
        let cwd = original_new_session_params.cwd.clone();
        let session = BridgeSession {
            id: session_id.clone(),
            original_new_session_params,
            cwd,
            selected_public_model_alias: None,
            selected_adapter_config_options: HashMap::new(),
            bound_gateway_session_id: None,
            state: BridgeSessionState::Unbound,
        };

        self.lock_sessions()
            .entry(tenant_id.clone())
            .or_default()
            .insert(session_id.clone(), session);
        session_id
    }

    /// Selects the public model alias before lazy binding begins.
    pub fn select_model(
        &self,
        tenant_id: &TenantId,
        session_id: &BridgeSessionId,
        public_model_alias: impl Into<String>,
    ) -> Result<BridgeSession, BridgeSessionError> {
        let mut sessions = self.lock_sessions();
        let session = session_mut(&mut sessions, tenant_id, session_id)?;
        if session.state != BridgeSessionState::Unbound {
            return Err(invalid_state("select model", session_id, session.state));
        }

        session.selected_public_model_alias = Some(public_model_alias.into());
        Ok(session.clone())
    }

    /// Updates the selected public alias after a bridge dispatcher has
    /// successfully applied a same-adapter model change to the backend.
    pub fn update_bound_model(
        &self,
        tenant_id: &TenantId,
        session_id: &BridgeSessionId,
        public_model_alias: impl Into<String>,
    ) -> Result<BridgeSession, BridgeSessionError> {
        let mut sessions = self.lock_sessions();
        let session = session_mut(&mut sessions, tenant_id, session_id)?;
        if session.state != BridgeSessionState::Bound {
            return Err(invalid_state(
                "update bound model",
                session_id,
                session.state,
            ));
        }
        session.selected_public_model_alias = Some(public_model_alias.into());
        Ok(session.clone())
    }

    /// Retain an accepted backend option so a recovered bridge session can
    /// recreate the same adapter configuration after a daemon restart.
    pub fn update_bound_adapter_config_option(
        &self,
        tenant_id: &TenantId,
        session_id: &BridgeSessionId,
        config_id: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<BridgeSession, BridgeSessionError> {
        let mut sessions = self.lock_sessions();
        let session = session_mut(&mut sessions, tenant_id, session_id)?;
        if session.state != BridgeSessionState::Bound {
            return Err(invalid_state(
                "update adapter config option",
                session_id,
                session.state,
            ));
        }
        session
            .selected_adapter_config_options
            .insert(config_id.into(), value.into());
        Ok(session.clone())
    }

    /// Stores one adapter configuration choice before binding. The bridge
    /// validates the public option before it reaches this transport-agnostic
    /// store; this type only enforces the lazy-binding lifecycle.
    pub fn select_adapter_config_option(
        &self,
        tenant_id: &TenantId,
        session_id: &BridgeSessionId,
        config_id: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<BridgeSession, BridgeSessionError> {
        let mut sessions = self.lock_sessions();
        let session = session_mut(&mut sessions, tenant_id, session_id)?;
        if session.state != BridgeSessionState::Unbound {
            return Err(invalid_state(
                "select adapter config option",
                session_id,
                session.state,
            ));
        }
        session
            .selected_adapter_config_options
            .insert(config_id.into(), value.into());
        Ok(session.clone())
    }

    /// Registers an already-created native ACPX gateway session behind a
    /// fresh virtual bridge id. This is used for `session/fork`: the
    /// backend has made the real fork, while the bridge must keep the
    /// native id private and give clients another virtual id.
    pub fn register_bound(
        &self,
        tenant_id: &TenantId,
        original_new_session_params: NewSessionParams,
        selected_public_model_alias: Option<String>,
        selected_adapter_config_options: HashMap<String, String>,
        bound_gateway_session_id: impl Into<String>,
    ) -> BridgeSessionId {
        let session_id = BridgeSessionId(uuid::Uuid::new_v4().to_string());
        let cwd = original_new_session_params.cwd.clone();
        let session = BridgeSession {
            id: session_id.clone(),
            original_new_session_params,
            cwd,
            selected_public_model_alias,
            selected_adapter_config_options,
            bound_gateway_session_id: Some(bound_gateway_session_id.into()),
            state: BridgeSessionState::Bound,
        };
        self.lock_sessions()
            .entry(tenant_id.clone())
            .or_default()
            .insert(session_id.clone(), session);
        session_id
    }

    /// Restore a bridge-visible session id after the native gateway session
    /// has recovered from persistence. Existing entries are never replaced.
    pub fn restore_bound(
        &self,
        tenant_id: &TenantId,
        session_id: BridgeSessionId,
        original_new_session_params: NewSessionParams,
        selected_public_model_alias: Option<String>,
        selected_adapter_config_options: HashMap<String, String>,
        bound_gateway_session_id: impl Into<String>,
    ) -> Result<(), BridgeSessionError> {
        let mut sessions = self.lock_sessions();
        let entries = sessions.entry(tenant_id.clone()).or_default();
        if let Some(existing) = entries.get(&session_id) {
            return Err(invalid_state(
                "restore binding",
                &session_id,
                existing.state,
            ));
        }
        let cwd = original_new_session_params.cwd.clone();
        entries.insert(
            session_id.clone(),
            BridgeSession {
                id: session_id,
                original_new_session_params,
                cwd,
                selected_public_model_alias,
                selected_adapter_config_options,
                bound_gateway_session_id: Some(bound_gateway_session_id.into()),
                state: BridgeSessionState::Bound,
            },
        );
        Ok(())
    }

    /// Atomically elects one caller to perform lazy binding.
    pub fn begin_binding(
        &self,
        tenant_id: &TenantId,
        session_id: &BridgeSessionId,
    ) -> Result<BindingClaim, BridgeSessionError> {
        let mut sessions = self.lock_sessions();
        let session = session_mut(&mut sessions, tenant_id, session_id)?;
        Ok(match session.state {
            BridgeSessionState::Unbound => {
                session.state = BridgeSessionState::Binding;
                BindingClaim::Owner
            }
            BridgeSessionState::Binding => BindingClaim::Binding,
            BridgeSessionState::Bound => BindingClaim::Bound,
            BridgeSessionState::Failed => BindingClaim::Failed,
        })
    }

    /// Marks binding successful. Only the current binding owner may do this.
    pub fn finish_binding(
        &self,
        tenant_id: &TenantId,
        session_id: &BridgeSessionId,
        bound_gateway_session_id: impl Into<String>,
    ) -> Result<BridgeSession, BridgeSessionError> {
        let mut sessions = self.lock_sessions();
        let session = session_mut(&mut sessions, tenant_id, session_id)?;
        if session.state != BridgeSessionState::Binding {
            return Err(invalid_state("finish binding", session_id, session.state));
        }
        session.bound_gateway_session_id = Some(bound_gateway_session_id.into());
        session.state = BridgeSessionState::Bound;
        Ok(session.clone())
    }

    /// Marks binding unsuccessful. Only the current binding owner may do this.
    pub fn fail_binding(
        &self,
        tenant_id: &TenantId,
        session_id: &BridgeSessionId,
    ) -> Result<BridgeSession, BridgeSessionError> {
        self.transition_from_binding(
            tenant_id,
            session_id,
            BridgeSessionState::Failed,
            "fail binding",
        )
    }

    /// Removes a bridge session from one tenant only.
    pub fn remove(
        &self,
        tenant_id: &TenantId,
        session_id: &BridgeSessionId,
    ) -> Option<BridgeSession> {
        self.lock_sessions().get_mut(tenant_id)?.remove(session_id)
    }

    /// Returns a snapshot of a bridge session from one tenant only.
    pub fn get(&self, tenant_id: &TenantId, session_id: &BridgeSessionId) -> Option<BridgeSession> {
        self.lock_sessions()
            .get(tenant_id)?
            .get(session_id)
            .cloned()
    }

    /// Resolves a bridge-visible id to the bound ACPX gateway id once lazy
    /// binding has completed.
    pub fn bound_gateway_session_id(
        &self,
        tenant_id: &TenantId,
        session_id: &BridgeSessionId,
    ) -> Option<String> {
        self.get(tenant_id, session_id)?.bound_gateway_session_id
    }

    /// Reverse lookup used by live transport forwarding to rewrite ACPX
    /// gateway session ids back into the bridge-visible virtual ids.
    pub fn find_by_bound_gateway_session_id(
        &self,
        tenant_id: &TenantId,
        bound_gateway_session_id: &str,
    ) -> Option<BridgeSessionId> {
        self.lock_sessions()
            .get(tenant_id)?
            .iter()
            .find_map(|(id, session)| {
                (session.bound_gateway_session_id.as_deref() == Some(bound_gateway_session_id))
                    .then(|| id.clone())
            })
    }

    fn transition_from_binding(
        &self,
        tenant_id: &TenantId,
        session_id: &BridgeSessionId,
        next_state: BridgeSessionState,
        operation: &'static str,
    ) -> Result<BridgeSession, BridgeSessionError> {
        let mut sessions = self.lock_sessions();
        let session = session_mut(&mut sessions, tenant_id, session_id)?;
        if session.state != BridgeSessionState::Binding {
            return Err(invalid_state(operation, session_id, session.state));
        }

        session.state = next_state;
        Ok(session.clone())
    }

    fn lock_sessions(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<TenantId, HashMap<BridgeSessionId, BridgeSession>>> {
        self.sessions
            .lock()
            .expect("bridge session store mutex must not be poisoned")
    }
}

fn session_mut<'a>(
    sessions: &'a mut HashMap<TenantId, HashMap<BridgeSessionId, BridgeSession>>,
    tenant_id: &TenantId,
    session_id: &BridgeSessionId,
) -> Result<&'a mut BridgeSession, BridgeSessionError> {
    sessions
        .get_mut(tenant_id)
        .and_then(|tenant_sessions| tenant_sessions.get_mut(session_id))
        .ok_or_else(|| not_found(tenant_id, session_id))
}

fn not_found(tenant_id: &TenantId, session_id: &BridgeSessionId) -> BridgeSessionError {
    BridgeSessionError::NotFound {
        tenant_id: tenant_id.0.clone(),
        session_id: session_id.0.clone(),
    }
}

fn invalid_state(
    operation: &'static str,
    session_id: &BridgeSessionId,
    state: BridgeSessionState,
) -> BridgeSessionError {
    BridgeSessionError::InvalidState {
        operation,
        session_id: session_id.0.clone(),
        state,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;

    fn params(cwd: &str) -> NewSessionParams {
        NewSessionParams {
            cwd: cwd.to_string(),
            mcp_servers: vec![serde_json::json!({"name": "filesystem"})],
            acpx: None,
            rest: serde_json::Map::from_iter([(
                "futureOption".to_string(),
                serde_json::json!({"preserved": true}),
            )]),
        }
    }

    #[test]
    fn register_preserves_original_params_and_scopes_sessions_by_tenant() {
        let store = BridgeSessionStore::new();
        let tenant_a = TenantId::from("tenant-a");
        let tenant_b = TenantId::from("tenant-b");
        let original = params("/workspace");

        let session_id = store.register(&tenant_a, original.clone());
        let session = store
            .get(&tenant_a, &session_id)
            .expect("registered session");

        assert_eq!(session.id, session_id);
        assert_eq!(session.cwd, "/workspace");
        assert_eq!(session.original_new_session_params.cwd, original.cwd);
        assert_eq!(
            session.original_new_session_params.rest, original.rest,
            "unknown session/new fields must survive lazy binding"
        );
        assert_eq!(session.state, BridgeSessionState::Unbound);
        assert!(store.get(&tenant_b, &session_id).is_none());
    }

    #[test]
    fn model_selection_is_only_allowed_before_binding() {
        let store = BridgeSessionStore::new();
        let tenant = TenantId::from("tenant-a");
        let session_id = store.register(&tenant, params("/workspace"));

        let selected = store
            .select_model(&tenant, &session_id, "claude/sonnet")
            .expect("unbound session accepts model selection");
        assert_eq!(
            selected.selected_public_model_alias.as_deref(),
            Some("claude/sonnet")
        );

        assert_eq!(
            store.begin_binding(&tenant, &session_id).unwrap(),
            BindingClaim::Owner
        );
        assert!(matches!(
            store.select_model(&tenant, &session_id, "codex/gpt-5"),
            Err(BridgeSessionError::InvalidState {
                state: BridgeSessionState::Binding,
                ..
            })
        ));
    }

    #[test]
    fn adapter_config_selection_is_retained_until_binding() {
        let store = BridgeSessionStore::new();
        let tenant = TenantId::from("tenant-a");
        let session_id = store.register(&tenant, params("/workspace"));
        let session = store
            .select_adapter_config_option(&tenant, &session_id, "permissionMode", "acceptEdits")
            .unwrap();
        assert_eq!(
            session
                .selected_adapter_config_options
                .get("permissionMode")
                .map(String::as_str),
            Some("acceptEdits")
        );
    }

    #[test]
    fn binding_lifecycle_is_one_way_and_has_no_backend_id() {
        let store = BridgeSessionStore::new();
        let tenant = TenantId::from("tenant-a");
        let session_id = store.register(&tenant, params("/workspace"));

        assert_eq!(
            store.begin_binding(&tenant, &session_id).unwrap(),
            BindingClaim::Owner
        );
        let bound = store
            .finish_binding(&tenant, &session_id, "native-session")
            .unwrap();
        assert_eq!(bound.state, BridgeSessionState::Bound);
        assert_eq!(
            bound.bound_gateway_session_id.as_deref(),
            Some("native-session")
        );
        assert_eq!(
            store.begin_binding(&tenant, &session_id).unwrap(),
            BindingClaim::Bound
        );
        assert!(matches!(
            store.fail_binding(&tenant, &session_id),
            Err(BridgeSessionError::InvalidState {
                state: BridgeSessionState::Bound,
                ..
            })
        ));
    }

    #[test]
    fn concurrent_first_prompts_have_one_binding_owner() {
        const CALLERS: usize = 16;

        let store = Arc::new(BridgeSessionStore::new());
        let tenant = TenantId::from("tenant-a");
        let session_id = store.register(&tenant, params("/workspace"));
        let barrier = Arc::new(Barrier::new(CALLERS));

        let handles: Vec<_> = (0..CALLERS)
            .map(|_| {
                let store = Arc::clone(&store);
                let tenant = tenant.clone();
                let session_id = session_id.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    store.begin_binding(&tenant, &session_id).unwrap()
                })
            })
            .collect();

        let claims: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("binding caller panicked"))
            .collect();
        assert_eq!(
            claims
                .iter()
                .filter(|claim| **claim == BindingClaim::Owner)
                .count(),
            1
        );
        assert_eq!(
            claims
                .iter()
                .filter(|claim| **claim == BindingClaim::Binding)
                .count(),
            CALLERS - 1
        );
        assert_eq!(
            store.get(&tenant, &session_id).unwrap().state,
            BridgeSessionState::Binding
        );
    }

    #[test]
    fn failed_binding_is_observable_and_removable() {
        let store = BridgeSessionStore::new();
        let tenant = TenantId::from("tenant-a");
        let session_id = store.register(&tenant, params("/workspace"));

        assert_eq!(
            store.begin_binding(&tenant, &session_id).unwrap(),
            BindingClaim::Owner
        );
        assert_eq!(
            store.fail_binding(&tenant, &session_id).unwrap().state,
            BridgeSessionState::Failed
        );
        assert_eq!(
            store.begin_binding(&tenant, &session_id).unwrap(),
            BindingClaim::Failed
        );
        assert_eq!(
            store.remove(&tenant, &session_id).unwrap().state,
            BridgeSessionState::Failed
        );
        assert!(store.get(&tenant, &session_id).is_none());
    }

    #[test]
    fn fork_registration_hides_native_gateway_identity() {
        let store = BridgeSessionStore::new();
        let tenant = TenantId::from("tenant-a");
        let public_id = store.register_bound(
            &tenant,
            params("/workspace"),
            Some("codex/gpt-5.5".to_string()),
            HashMap::new(),
            "native-gateway-fork",
        );
        let fork = store
            .get(&tenant, &public_id)
            .expect("forked bridge session");
        assert_eq!(fork.state, BridgeSessionState::Bound);
        assert_eq!(
            fork.bound_gateway_session_id.as_deref(),
            Some("native-gateway-fork")
        );
        assert_ne!(fork.id.0, "native-gateway-fork");
    }
}
