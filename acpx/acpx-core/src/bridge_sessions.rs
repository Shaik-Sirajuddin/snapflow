//! Transport-agnostic virtual sessions used by an ACP compatibility bridge.
//!
//! A bridge session is deliberately separate from [`crate::SessionRegistry`]:
//! it exists before an adapter session has been created, so it must never
//! invent or store a backend session id. The first prompt claims binding
//! ownership atomically; later prompts can observe that binding is in flight.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
    /// **`virtual_and_pinned_resource_limits`.** When this virtual
    /// session was registered -- backs [`BridgeSessionStore::
    /// reap_stale_unbound`]'s TTL check. Not persisted (bridge sessions
    /// that survive a restart are re-registered via
    /// [`BridgeSessionStore::restore_bound`], always already `Bound`,
    /// so they are never reap candidates regardless of this field's
    /// value after a restart).
    created_at: Instant,
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
    #[error("tenant {tenant_id} already has {current} of at most {limit} virtual bridge sessions")]
    VirtualSessionQuotaExceeded {
        tenant_id: String,
        current: usize,
        limit: usize,
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
        self.try_register(tenant_id, original_new_session_params, None)
            .expect("register: no limit passed, so a quota error is impossible")
    }

    /// Same as [`Self::register`], but enforces
    /// `max_virtual_sessions_per_tenant` (`None` means unlimited, matching
    /// [`Self::register`]'s unchanged behavior). **`virtual_and_pinned_
    /// resource_limits`.** The quota counts every session currently in
    /// this tenant's map regardless of state (`Unbound`/`Binding`/`Bound`/
    /// `Failed`) -- a `Bound` session still holds a real backend session
    /// alive, and a `Failed` one is only removed by an explicit client
    /// retry/cleanup, so both remain real resource consumers until
    /// removed.
    pub fn try_register(
        &self,
        tenant_id: &TenantId,
        original_new_session_params: NewSessionParams,
        max_per_tenant: Option<usize>,
    ) -> Result<BridgeSessionId, BridgeSessionError> {
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
            created_at: Instant::now(),
        };

        let mut sessions = self.lock_sessions();
        let tenant_sessions = sessions.entry(tenant_id.clone()).or_default();
        if let Some(limit) = max_per_tenant {
            let current = tenant_sessions.len();
            if current >= limit {
                return Err(BridgeSessionError::VirtualSessionQuotaExceeded {
                    tenant_id: tenant_id.0.clone(),
                    current,
                    limit,
                });
            }
        }
        tenant_sessions.insert(session_id.clone(), session);
        Ok(session_id)
    }

    /// Count of virtual sessions currently registered for one tenant --
    /// the same count [`Self::try_register`]'s quota check uses.
    pub fn count_for_tenant(&self, tenant_id: &TenantId) -> usize {
        self.lock_sessions()
            .get(tenant_id)
            .map(HashMap::len)
            .unwrap_or(0)
    }

    /// **`virtual_and_pinned_resource_limits`.** Removes every `Unbound`
    /// virtual session across every tenant whose age exceeds `ttl`.
    /// Deliberately scoped to `Unbound` only: a session in `Binding`
    /// might have a real backend-creation round trip in flight (removing
    /// it here could orphan that in-flight work with no way to ever
    /// observe its outcome), and `Bound`/`Failed` sessions are addressed
    /// by (respectively) the native session's own idle-TTL reaper and an
    /// explicit client retry -- an `Unbound` session that a client never
    /// followed up on (never sent a first prompt) is the one case with
    /// no other lifecycle owner at all. Returns the number removed.
    pub fn reap_stale_unbound(&self, now: Instant, ttl: std::time::Duration) -> usize {
        let mut sessions = self.lock_sessions();
        let mut removed = 0;
        for tenant_sessions in sessions.values_mut() {
            tenant_sessions.retain(|_, session| {
                let stale = session.state == BridgeSessionState::Unbound
                    && now.saturating_duration_since(session.created_at) >= ttl;
                if stale {
                    removed += 1;
                }
                !stale
            });
        }
        removed
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
            created_at: Instant::now(),
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
                created_at: Instant::now(),
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
        // Self-heals on poison instead of permanently wedging every
        // future bridge-session operation this process ever makes: a
        // panic mid-critical-section here is already a bug worth its own
        // fix, but a poisoned `std::sync::Mutex` otherwise means every
        // caller's own `.expect()` panics forever after, turning one
        // isolated panic into a permanent, process-wide "no bridge
        // session can ever be read or written again" outage -- strictly
        // worse than proceeding with whatever (still internally
        // consistent, plain-data `HashMap`) state the guard protects.
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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

    #[test]
    fn try_register_enforces_the_per_tenant_quota_and_is_not_shared_across_tenants() {
        let store = BridgeSessionStore::new();
        let tenant_a = TenantId::from("tenant-a");
        let tenant_b = TenantId::from("tenant-b");

        store
            .try_register(&tenant_a, params("/workspace"), Some(1))
            .expect("first session under quota");
        let rejected = store.try_register(&tenant_a, params("/workspace"), Some(1));
        assert!(matches!(
            rejected,
            Err(BridgeSessionError::VirtualSessionQuotaExceeded {
                current: 1,
                limit: 1,
                ..
            })
        ));

        // tenant-b's own quota is independent of tenant-a's usage.
        store
            .try_register(&tenant_b, params("/workspace"), Some(1))
            .expect("a different tenant has its own separate quota");
    }

    #[test]
    fn register_never_enforces_a_quota() {
        let store = BridgeSessionStore::new();
        let tenant = TenantId::from("tenant-a");
        // The legacy unlimited `register` entry point must keep working
        // unchanged for every pre-existing caller that never opts into a
        // quota.
        for _ in 0..5 {
            store.register(&tenant, params("/workspace"));
        }
        assert_eq!(store.count_for_tenant(&tenant), 5);
    }

    #[test]
    fn reap_stale_unbound_removes_only_expired_unbound_sessions() {
        let store = BridgeSessionStore::new();
        let tenant = TenantId::from("tenant-a");
        let stale_unbound = store.register(&tenant, params("/workspace"));
        let bound = store.register(&tenant, params("/workspace"));
        store.begin_binding(&tenant, &bound).unwrap();
        store
            .finish_binding(&tenant, &bound, "native-session")
            .unwrap();

        // `stale_unbound`/`bound` are registered, then a real delay
        // elapses, then `fresh_unbound` is registered -- a TTL between
        // the two ages must reap only the session actually older than
        // it, proving this reads each session's own age rather than a
        // single global cutoff.
        std::thread::sleep(std::time::Duration::from_millis(30));
        let fresh_unbound = store.register(&tenant, params("/workspace"));

        let removed =
            store.reap_stale_unbound(Instant::now(), std::time::Duration::from_millis(15));
        assert_eq!(removed, 1);
        assert!(store.get(&tenant, &stale_unbound).is_none());
        assert!(store.get(&tenant, &fresh_unbound).is_some());
        assert!(store.get(&tenant, &bound).is_some());
    }
}
