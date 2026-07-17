//! Gateway session id -> (agent, backend session id) mapping. Phase 2
//! step 8 in the phased plan; Phase 1's single-agent spike doesn't need
//! this (see `acpx-server`'s Phase 1 passthrough), but it's cheap to stand
//! up now since it has no dependency on multi-agent routing.

use acpx_proto::session::GatewaySessionId;
use std::collections::HashMap;
use std::time::Instant;

use crate::LifecycleConfig;

/// **Phase A (`acpx-tenant-isolation`, see
/// `memory/acpx/gen/plans/acpx-tenant-isolation/01-architecture.md`).**
/// A self-declared session-namespace partition key -- *not* an
/// authenticated identity (see that plan's `00-goal.md`, "Why auth is
/// out of scope"). Every `SessionRegistry` method below takes one as its
/// first argument; two different `TenantId`s never see or collide with
/// each other's sessions even if they otherwise use identical gateway
/// session id strings (impossible in practice, since those are random
/// UUIDs, but the map is nested by tenant regardless -- a structural
/// invariant, not a reliance on UUID non-collision).
///
/// This phase only introduces the type and the nested map; every call
/// site in `router.rs` still passes [`TenantId::default_tenant`]
/// unconditionally, so behavior is byte-for-byte unchanged until a later
/// phase actually threads a real per-connection tenant id in from
/// `acpx-server`'s transports (`X-Acpx-Tenant` header).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TenantId(pub String);

impl TenantId {
    /// The tenant every pre-existing (tenant-unaware) caller implicitly
    /// uses -- keeps every deployment/test that never opts into tenant
    /// scoping working unchanged.
    pub fn default_tenant() -> Self {
        TenantId("default".to_string())
    }
}

impl Default for TenantId {
    fn default() -> Self {
        Self::default_tenant()
    }
}

impl From<&str> for TenantId {
    fn from(value: &str) -> Self {
        TenantId(value.to_string())
    }
}

impl From<String> for TenantId {
    fn from(value: String) -> Self {
        TenantId(value)
    }
}

impl std::fmt::Display for TenantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

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
    /// Monotonic timestamps keep retention independent of wall-clock jumps.
    pub created_at: Instant,
    pub last_activity_at: Instant,
    /// A reaper must never evict a session while a backend operation is
    /// executing against it.
    pub in_flight: usize,
    /// **`active_turn_deadline`.** When `in_flight` most recently
    /// transitioned from `0` to non-zero -- `None` whenever `in_flight`
    /// is `0`. Backs `SessionRegistry::stuck_in_flight_candidates`: a
    /// turn that has been continuously in-flight since before
    /// `LifecycleConfig::active_turn_deadline` ago is a candidate for
    /// bounded cancellation rather than an indefinite reaper skip. Kept
    /// separate from `last_activity_at` (which `set_in_flight(0)`
    /// refreshes) because the deadline measures how long the *current*
    /// turn has run, not how recently the session was last touched.
    pub in_flight_since: Option<Instant>,
    /// Explicit retention override controlled by ACPX administration.
    pub pinned: bool,
    /// **`retention_administration`.** Per-session idle-TTL override, set
    /// via `session/retention/set_ttl` -- `None` (the default) means
    /// "use `LifecycleConfig::idle_session_ttl` like every other
    /// session", `Some(duration)` overrides it for this session alone
    /// (shorter *or* longer than the deployment default; an operator
    /// might want a long-lived session to survive an unusually long idle
    /// gap without pinning it outright, or a short-lived one reaped
    /// sooner than the default). Deliberately independent of `pinned`:
    /// a session can have a custom TTL and still eventually be reaped by
    /// it, whereas `pinned` exempts a session from idle reaping
    /// entirely regardless of any TTL.
    pub custom_idle_ttl: Option<std::time::Duration>,
}

#[derive(Debug, Default)]
pub struct SessionRegistry {
    /// Nested by tenant (Phase A, `acpx-tenant-isolation`) -- outer key
    /// is a [`TenantId`], inner map is the pre-existing
    /// `gateway_session_id -> SessionEntry` index, now scoped so two
    /// tenants can never see or overwrite each other's entries even if
    /// (hypothetically) they shared an inner id string.
    sessions: HashMap<TenantId, HashMap<String, SessionEntry>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a newly-created session, minting a fresh gateway session id.
    pub fn register(
        &mut self,
        tenant_id: &TenantId,
        agent_id: impl Into<String>,
        backend_session_id: BackendSessionId,
        profile_name: Option<String>,
        cwd: Option<String>,
    ) -> GatewaySessionId {
        let gateway_id = GatewaySessionId(uuid::Uuid::new_v4().to_string());
        self.register_with_id(
            tenant_id,
            gateway_id,
            agent_id,
            backend_session_id,
            profile_name,
            cwd,
        )
    }

    /// Same as [`Self::register`], but with an already-minted gateway id
    /// rather than generating a fresh one. Backs per-session backend
    /// process isolation (`ACPX_SESSION_PROCESS_ISOLATION`, see
    /// `Router::dispatch_session_new`): that feature must fold the
    /// session's own gateway id into its supervisor key *before* the
    /// backend is even spawned, so the id has to be minted up front
    /// rather than only after a successful `session/new` round trip like
    /// [`Self::register`] does.
    pub fn register_with_id(
        &mut self,
        tenant_id: &TenantId,
        gateway_id: GatewaySessionId,
        agent_id: impl Into<String>,
        backend_session_id: BackendSessionId,
        profile_name: Option<String>,
        cwd: Option<String>,
    ) -> GatewaySessionId {
        self.sessions.entry(tenant_id.clone()).or_default().insert(
            gateway_id.0.clone(),
            SessionEntry {
                agent_id: agent_id.into(),
                backend_session_id,
                profile_name,
                cwd,
                created_at: Instant::now(),
                last_activity_at: Instant::now(),
                in_flight: 0,
                in_flight_since: None,
                pinned: false,
                custom_idle_ttl: None,
            },
        );
        gateway_id
    }

    pub fn resolve(
        &self,
        tenant_id: &TenantId,
        gateway_id: &GatewaySessionId,
    ) -> Option<&SessionEntry> {
        self.sessions.get(tenant_id)?.get(&gateway_id.0)
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
    pub fn insert(
        &mut self,
        tenant_id: &TenantId,
        gateway_id: GatewaySessionId,
        entry: SessionEntry,
    ) {
        self.sessions
            .entry(tenant_id.clone())
            .or_default()
            .insert(gateway_id.0, entry);
    }

    /// Refreshes an existing session's activity deadline.
    pub fn touch(&mut self, tenant_id: &TenantId, gateway_id: &GatewaySessionId) -> bool {
        let Some(entry) = self
            .sessions
            .get_mut(tenant_id)
            .and_then(|sessions| sessions.get_mut(&gateway_id.0))
        else {
            return false;
        };
        entry.last_activity_at = Instant::now();
        true
    }

    /// Marks a session as executing or finished executing backend work.
    pub fn set_in_flight(
        &mut self,
        tenant_id: &TenantId,
        gateway_id: &GatewaySessionId,
        in_flight: usize,
    ) -> bool {
        let Some(entry) = self
            .sessions
            .get_mut(tenant_id)
            .and_then(|sessions| sessions.get_mut(&gateway_id.0))
        else {
            return false;
        };
        let was_idle = entry.in_flight == 0;
        entry.in_flight = in_flight;
        if in_flight == 0 {
            entry.last_activity_at = Instant::now();
            entry.in_flight_since = None;
        } else if was_idle {
            // Only stamp on a genuine `0 -> non-zero` transition, not on
            // a same-turn re-assertion (e.g. a caller re-marking an
            // already in-flight session), so the deadline measures the
            // current turn's actual start.
            entry.in_flight_since = Some(Instant::now());
        }
        true
    }

    /// Sets the explicit retention override for one tenant-scoped session.
    pub fn set_pinned(
        &mut self,
        tenant_id: &TenantId,
        gateway_id: &GatewaySessionId,
        pinned: bool,
    ) -> bool {
        let Some(entry) = self
            .sessions
            .get_mut(tenant_id)
            .and_then(|sessions| sessions.get_mut(&gateway_id.0))
        else {
            return false;
        };
        entry.pinned = pinned;
        entry.last_activity_at = Instant::now();
        true
    }

    /// Sets (or clears, with `ttl: None`) the per-session idle-TTL
    /// override for one tenant-scoped session. See
    /// [`SessionEntry::custom_idle_ttl`]'s doc comment.
    pub fn set_custom_ttl(
        &mut self,
        tenant_id: &TenantId,
        gateway_id: &GatewaySessionId,
        ttl: Option<std::time::Duration>,
    ) -> bool {
        let Some(entry) = self
            .sessions
            .get_mut(tenant_id)
            .and_then(|sessions| sessions.get_mut(&gateway_id.0))
        else {
            return false;
        };
        entry.custom_idle_ttl = ttl;
        true
    }

    /// Count of currently-pinned sessions for one tenant -- backs the
    /// `session/retention/pin` pin-quota check
    /// (`LifecycleConfig::max_pinned_sessions_per_tenant`).
    pub fn pinned_count(&self, tenant_id: &TenantId) -> usize {
        self.sessions
            .get(tenant_id)
            .map(|sessions| sessions.values().filter(|entry| entry.pinned).count())
            .unwrap_or(0)
    }

    /// Every `(gateway_session_id, entry)` pair for one tenant, for the
    /// `session/retention/list` tenant-scoped inspection method.
    /// Snapshotted as owned `GatewaySessionId`s (not borrowed) so a
    /// caller can format a response without holding this registry's
    /// borrow across it.
    pub fn list_for_tenant(&self, tenant_id: &TenantId) -> Vec<(GatewaySessionId, SessionEntry)> {
        self.sessions
            .get(tenant_id)
            .map(|sessions| {
                sessions
                    .iter()
                    .map(|(id, entry)| (GatewaySessionId(id.clone()), entry.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Lists sessions eligible for lifecycle cleanup without mutating them.
    /// Callers are responsible for marking/closing each candidate before
    /// removal so a concurrent operation cannot race a backend close.
    pub fn reap_candidates(
        &self,
        now: Instant,
        lifecycle: &LifecycleConfig,
    ) -> Vec<(TenantId, GatewaySessionId)> {
        self.sessions
            .iter()
            .flat_map(|(tenant, sessions)| {
                sessions.iter().filter_map(move |(id, entry)| {
                    if entry.pinned || entry.in_flight != 0 {
                        return None;
                    }
                    let idle = now.saturating_duration_since(entry.last_activity_at);
                    let effective_idle_ttl =
                        entry.custom_idle_ttl.unwrap_or(lifecycle.idle_session_ttl);
                    let absolute = lifecycle
                        .absolute_session_ttl
                        .is_some_and(|ttl| now.saturating_duration_since(entry.created_at) >= ttl);
                    (idle >= effective_idle_ttl || absolute)
                        .then(|| (tenant.clone(), GatewaySessionId(id.clone())))
                })
            })
            .collect()
    }

    /// **`active_turn_deadline`.** Lists sessions whose *current* turn
    /// has been continuously in-flight for at least `deadline` -- a
    /// no-op (always empty) when `LifecycleConfig::active_turn_deadline`
    /// is `None`, matching the unbounded-skip behavior that predates
    /// this field. Disjoint from [`Self::reap_candidates`] by
    /// construction: that method only ever selects `in_flight == 0`
    /// entries, this one only ever selects `in_flight != 0` entries with
    /// a recorded start time, so a caller can run both passes over the
    /// same tick without double-selecting a session.
    pub fn stuck_in_flight_candidates(
        &self,
        now: Instant,
        lifecycle: &LifecycleConfig,
    ) -> Vec<(TenantId, GatewaySessionId)> {
        let Some(deadline) = lifecycle.active_turn_deadline else {
            return Vec::new();
        };
        self.sessions
            .iter()
            .flat_map(|(tenant, sessions)| {
                sessions.iter().filter_map(move |(id, entry)| {
                    let since = entry.in_flight_since?;
                    if entry.in_flight == 0 {
                        return None;
                    }
                    (now.saturating_duration_since(since) >= deadline)
                        .then(|| (tenant.clone(), GatewaySessionId(id.clone())))
                })
            })
            .collect()
    }

    /// **`connector_reference_lifecycle`.** Count of currently-live
    /// sessions (any tenant) whose `agent_id` (a supervisor key -- a
    /// bare registered agent id in native mode, or `profile:<name>[...]`
    /// once resolved through a profile, see `Router::resolve_profile`)
    /// equals `supervisor_key`. Backs `Router`'s connector-idle-shutdown
    /// reference counting: a shared backend process under a given key is
    /// only a real idle-shutdown candidate once this reaches zero.
    pub fn count_by_agent_id(&self, supervisor_key: &str) -> usize {
        self.sessions
            .values()
            .map(|tenant_sessions| {
                tenant_sessions
                    .values()
                    .filter(|entry| entry.agent_id == supervisor_key)
                    .count()
            })
            .sum()
    }

    pub fn remove(
        &mut self,
        tenant_id: &TenantId,
        gateway_id: &GatewaySessionId,
    ) -> Option<SessionEntry> {
        let inner = self.sessions.get_mut(tenant_id)?;
        let removed = inner.remove(&gateway_id.0);
        // Prune the now-empty tenant entry outright (`tenant_namespace_
        // governance` hardening item, `acpx-tenant-isolation` plan): a
        // caller-controlled `X-Acpx-Tenant` value mints a fresh outer map
        // key on first use; without this, closing every session under it
        // still leaves an empty `HashMap` sitting in `self.sessions`
        // forever, so an attacker (or just a buggy client) rotating
        // arbitrary tenant strings could grow this map unboundedly even
        // while never holding more than one live session at a time.
        // `default_tenant()` is exempt -- it is the implicit tenant every
        // unscoped caller uses and should always resolve to *a* (empty is
        // fine) map rather than be re-created from scratch mid-request.
        if inner.is_empty() && *tenant_id != TenantId::default_tenant() {
            self.sessions.remove(tenant_id);
        }
        removed
    }

    /// Number of distinct tenant namespaces currently tracked (including
    /// ones with zero live sessions, which should not normally accumulate
    /// post-[`Self::remove`]'s pruning -- exposed for governance/ops
    /// visibility and test assertions, not on any hot path).
    pub fn tenant_count(&self) -> usize {
        self.sessions.len()
    }

    /// Aggregated `session/list` -- all live sessions across every backend.
    pub fn list(&self, tenant_id: &TenantId) -> impl Iterator<Item = (&String, &SessionEntry)> {
        self.sessions
            .get(tenant_id)
            .into_iter()
            .flat_map(|inner| inner.iter())
    }

    /// Number of live gateway sessions across every tenant.
    pub fn len(&self) -> usize {
        self.sessions.values().map(HashMap::len).sum()
    }

    /// Required alongside `len` by clippy's `len_without_is_empty` --
    /// also a legitimately useful check on its own (whether *any* tenant
    /// currently has a live session at all).
    pub fn is_empty(&self) -> bool {
        self.sessions.values().all(HashMap::is_empty)
    }

    /// Number of live gateway sessions owned by one tenant.
    pub fn len_for_tenant(&self, tenant_id: &TenantId) -> usize {
        self.sessions
            .get(tenant_id)
            .map(HashMap::len)
            .unwrap_or_default()
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
        tenant_id: &TenantId,
        agent_id: &str,
        backend_session_id: &str,
    ) -> Option<GatewaySessionId> {
        let inner = self.sessions.get(tenant_id)?;
        inner.iter().find_map(|(gid, entry)| {
            if entry.agent_id == agent_id && entry.backend_session_id.0 == backend_session_id {
                Some(GatewaySessionId(gid.clone()))
            } else {
                None
            }
        })
    }

    /// **Phase B (`acpx-tenant-isolation`), closes the real per-backend
    /// `session/list` cross-tenant leak flagged in this plan's
    /// `01-architecture.md`.** Unlike [`Self::find_by_backend`] (scoped to
    /// one caller-known tenant), this scans *every* tenant's submap to
    /// answer "does some tenant -- any tenant -- already own this exact
    /// `(agent_id, backend_session_id)` pair?", returning which one if so.
    /// A physical backend process is shared across every tenant using the
    /// same profile (see `01-architecture.md`'s "backend process sharing"
    /// section), so a backend's own `session/list` reply can legitimately
    /// include a session some *other* tenant created -- this is the check
    /// that lets the caller refuse to hand that session to the requesting
    /// tenant instead of silently adopting it.
    pub fn find_owner(&self, agent_id: &str, backend_session_id: &str) -> Option<&TenantId> {
        self.sessions.iter().find_map(|(tenant, inner)| {
            inner.values().find_map(|entry| {
                if entry.agent_id == agent_id && entry.backend_session_id.0 == backend_session_id {
                    Some(tenant)
                } else {
                    None
                }
            })
        })
    }

    /// Like [`Self::find_owner`], but also returns the matching
    /// [`GatewaySessionId`] rather than just which tenant owns it -- used
    /// by the phase-15 idle-scavenger background task
    /// ([`crate::router::backend_idle_scavenger`]), which has no
    /// per-call tenant context of its own (it runs once per physical
    /// backend process, which may be shared across tenants), so it must
    /// search across every tenant to find whichever one (if any) owns a
    /// given backend-native session id.
    pub fn find_by_backend_any_tenant(
        &self,
        agent_id: &str,
        backend_session_id: &str,
    ) -> Option<(TenantId, GatewaySessionId)> {
        self.sessions.iter().find_map(|(tenant, inner)| {
            inner.iter().find_map(|(gid, entry)| {
                if entry.agent_id == agent_id && entry.backend_session_id.0 == backend_session_id {
                    Some((tenant.clone(), GatewaySessionId(gid.clone())))
                } else {
                    None
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_resolve_round_trips() {
        let mut reg = SessionRegistry::new();
        let tenant = TenantId::default_tenant();
        let gid = reg.register(
            &tenant,
            "codex-acp",
            BackendSessionId("backend-1".to_string()),
            None,
            None,
        );
        let entry = reg.resolve(&tenant, &gid).expect("just registered");
        assert_eq!(entry.agent_id, "codex-acp");
        assert_eq!(entry.backend_session_id.0, "backend-1");
    }

    #[test]
    fn remove_forgets_the_session() {
        let mut reg = SessionRegistry::new();
        let tenant = TenantId::default_tenant();
        let gid = reg.register(
            &tenant,
            "codex-acp",
            BackendSessionId("backend-1".to_string()),
            None,
            None,
        );
        assert!(reg.remove(&tenant, &gid).is_some());
        assert!(reg.resolve(&tenant, &gid).is_none());
    }

    /// `tenant_namespace_governance` hardening: closing every session
    /// under a non-default tenant must not leave an empty map entry
    /// behind, otherwise a caller rotating arbitrary self-declared
    /// tenant strings could grow `sessions` unboundedly forever.
    #[test]
    fn removing_the_last_session_prunes_a_non_default_tenant_namespace() {
        let mut reg = SessionRegistry::new();
        let tenant = TenantId::from("acme");
        assert_eq!(reg.tenant_count(), 0);
        let gid = reg.register(
            &tenant,
            "codex-acp",
            BackendSessionId("backend-1".to_string()),
            None,
            None,
        );
        assert_eq!(reg.tenant_count(), 1);
        assert!(reg.remove(&tenant, &gid).is_some());
        assert_eq!(
            reg.tenant_count(),
            0,
            "the now-empty tenant namespace should be pruned, not retained"
        );
    }

    /// The implicit default tenant is exempt from pruning -- it should
    /// always resolve to *a* (possibly empty) namespace rather than be
    /// torn down and re-created on every session churn.
    #[test]
    fn removing_the_last_session_keeps_the_default_tenant_namespace() {
        let mut reg = SessionRegistry::new();
        let tenant = TenantId::default_tenant();
        let gid = reg.register(
            &tenant,
            "codex-acp",
            BackendSessionId("backend-1".to_string()),
            None,
            None,
        );
        assert!(reg.remove(&tenant, &gid).is_some());
        assert_eq!(
            reg.tenant_count(),
            1,
            "the default tenant namespace stays tracked even with zero sessions"
        );
    }

    #[test]
    fn find_by_backend_locates_an_already_registered_session() {
        let mut reg = SessionRegistry::new();
        let tenant = TenantId::default_tenant();
        let gid = reg.register(
            &tenant,
            "codex-acp",
            BackendSessionId("backend-1".to_string()),
            None,
            Some("/tmp".to_string()),
        );
        assert_eq!(
            reg.find_by_backend(&tenant, "codex-acp", "backend-1"),
            Some(gid)
        );
        assert_eq!(reg.find_by_backend(&tenant, "codex-acp", "backend-2"), None);
        assert_eq!(
            reg.find_by_backend(&tenant, "other-agent", "backend-1"),
            None
        );
    }

    /// **Phase A (`acpx-tenant-isolation`).** The core proof this phase
    /// exists for: two different tenants never see or collide with each
    /// other's sessions, even though `register` mints ids from the same
    /// global UUID space and both tenants use the exact same
    /// `agent_id`/`backend_session_id` pair.
    #[test]
    fn two_tenants_never_collide_even_with_identical_backend_identity() {
        let mut reg = SessionRegistry::new();
        let tenant_a = TenantId::from("tenant-a");
        let tenant_b = TenantId::from("tenant-b");

        let gid_a = reg.register(
            &tenant_a,
            "codex-acp",
            BackendSessionId("backend-1".to_string()),
            None,
            None,
        );
        let gid_b = reg.register(
            &tenant_b,
            "codex-acp",
            BackendSessionId("backend-1".to_string()),
            None,
            None,
        );

        // Each tenant resolves its own session fine...
        assert!(reg.resolve(&tenant_a, &gid_a).is_some());
        assert!(reg.resolve(&tenant_b, &gid_b).is_some());
        // ...but never the other tenant's, even if (contrived here) the
        // gateway id string were somehow reused -- resolving tenant A's
        // id under tenant B's namespace must miss entirely.
        assert!(reg.resolve(&tenant_b, &gid_a).is_none());
        assert!(reg.resolve(&tenant_a, &gid_b).is_none());

        // `find_by_backend` and `list` are also strictly tenant-scoped.
        assert_eq!(
            reg.find_by_backend(&tenant_a, "codex-acp", "backend-1"),
            Some(gid_a)
        );
        assert_eq!(
            reg.find_by_backend(&tenant_b, "codex-acp", "backend-1"),
            Some(gid_b)
        );
        assert_eq!(reg.list(&tenant_a).count(), 1);
        assert_eq!(reg.list(&tenant_b).count(), 1);
        assert_eq!(reg.list(&TenantId::from("tenant-c")).count(), 0);
    }

    /// **Phase B.** `find_owner` is the cross-tenant lookup the
    /// `session/list` leak fix relies on: it must find a session
    /// regardless of which tenant registered it, so a caller can detect
    /// "someone else already owns this" even without knowing who.
    #[test]
    fn find_owner_locates_a_session_regardless_of_which_tenant_registered_it() {
        let mut reg = SessionRegistry::new();
        let tenant_a = TenantId::from("tenant-a");
        reg.register(
            &tenant_a,
            "codex-acp",
            BackendSessionId("backend-1".to_string()),
            None,
            None,
        );
        assert_eq!(reg.find_owner("codex-acp", "backend-1"), Some(&tenant_a));
        assert_eq!(reg.find_owner("codex-acp", "backend-2"), None);
    }

    #[test]
    fn lengths_track_total_and_tenant_scoped_sessions() {
        let mut reg = SessionRegistry::new();
        let tenant_a = TenantId::from("tenant-a");
        let tenant_b = TenantId::from("tenant-b");
        reg.register(
            &tenant_a,
            "claude-agent-acp",
            BackendSessionId("a-1".to_string()),
            None,
            None,
        );
        reg.register(
            &tenant_b,
            "codex-acp",
            BackendSessionId("b-1".to_string()),
            None,
            None,
        );
        assert_eq!(reg.len(), 2);
        assert_eq!(reg.len_for_tenant(&tenant_a), 1);
        assert_eq!(reg.len_for_tenant(&TenantId::from("tenant-c")), 0);
    }

    #[test]
    fn reap_candidates_exclude_pinned_and_in_flight_sessions() {
        let mut reg = SessionRegistry::new();
        let tenant = TenantId::default_tenant();
        let idle = reg.register(
            &tenant,
            "agent",
            BackendSessionId("idle".to_string()),
            None,
            None,
        );
        let pinned = reg.register(
            &tenant,
            "agent",
            BackendSessionId("pinned".to_string()),
            None,
            None,
        );
        let active = reg.register(
            &tenant,
            "agent",
            BackendSessionId("active".to_string()),
            None,
            None,
        );
        reg.set_pinned(&tenant, &pinned, true);
        reg.set_in_flight(&tenant, &active, 1);

        let then = Instant::now() + std::time::Duration::from_secs(31 * 60);
        let candidates = reg.reap_candidates(then, &LifecycleConfig::default());
        assert_eq!(candidates, vec![(tenant, idle)]);
    }

    /// **`active_turn_deadline`.** `stuck_in_flight_candidates` only
    /// selects a session whose *current* turn has run at least the
    /// configured deadline, is disabled entirely (`None`) by default,
    /// and never overlaps `reap_candidates`' own selection (that method
    /// only ever selects `in_flight == 0` entries).
    #[test]
    fn stuck_in_flight_candidates_respects_the_configured_deadline() {
        let mut reg = SessionRegistry::new();
        let tenant = TenantId::default_tenant();
        let stuck = reg.register(
            &tenant,
            "agent",
            BackendSessionId("stuck".to_string()),
            None,
            None,
        );
        let idle = reg.register(
            &tenant,
            "agent",
            BackendSessionId("idle".to_string()),
            None,
            None,
        );
        reg.set_in_flight(&tenant, &stuck, 1);

        let deadline = std::time::Duration::from_secs(60);
        let default_lifecycle = LifecycleConfig::default();
        assert!(
            reg.stuck_in_flight_candidates(Instant::now() + deadline * 10, &default_lifecycle)
                .is_empty(),
            "disabled (None) active_turn_deadline must never select anything"
        );

        let lifecycle = LifecycleConfig {
            active_turn_deadline: Some(deadline),
            ..LifecycleConfig::default()
        };
        // Neither an idle (never in-flight) nor a freshly in-flight
        // session is a candidate yet.
        let too_soon = reg.stuck_in_flight_candidates(Instant::now(), &lifecycle);
        assert!(too_soon.is_empty());

        // Only `stuck` (in-flight since before this call) qualifies once
        // the deadline has passed; `idle` (never in-flight at all) never
        // does, no matter how far `now` is pushed forward.
        let candidates = reg.stuck_in_flight_candidates(Instant::now() + deadline * 10, &lifecycle);
        assert_eq!(candidates, vec![(tenant.clone(), stuck.clone())]);
        assert!(!candidates.contains(&(tenant.clone(), idle)));

        // Clearing in-flight (as `Router::cancel_stuck_turns` does after
        // delivering the cancellation) removes it from future candidacy.
        reg.set_in_flight(&tenant, &stuck, 0);
        assert!(reg
            .stuck_in_flight_candidates(Instant::now() + deadline * 10, &lifecycle)
            .is_empty());
    }
}
