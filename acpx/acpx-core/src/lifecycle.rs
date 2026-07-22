//! Native ACPX session lifecycle limits.
//!
//! This module is transport-agnostic. `acpx-server` reads deployment
//! configuration, while `Router` uses these limits before it creates a
//! backend session so a full gateway never spends connector capacity only to
//! reject the client afterward.

/// Resource bounds for live ACPX gateway sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleConfig {
    pub max_sessions_total: usize,
    pub max_sessions_per_tenant: usize,
    pub idle_session_ttl: std::time::Duration,
    pub unbound_bridge_session_ttl: std::time::Duration,
    pub absolute_session_ttl: Option<std::time::Duration>,
    /// **`retention_administration`.** Caps how many sessions one tenant
    /// may hold pinned (exempt from idle reaping) at once, via
    /// `session/retention/pin`. `None` (the default) means unlimited --
    /// unchanged behavior from before this field existed, since pinning
    /// was previously only reachable through `Router::set_session_pinned`
    /// as an in-process-only seam with no client-facing quota concern.
    /// `Some(0)` is deliberately rejected by `validate` below (use a
    /// deployment that simply never enables the retention-administration
    /// JSON-RPC methods instead of a zero quota, which would be a
    /// confusing way to spell "no pinning allowed").
    pub max_pinned_sessions_per_tenant: Option<usize>,
    /// **`connector_reference_lifecycle`.** How long a shared backend
    /// process (a supervisor key with zero currently-referencing live
    /// sessions) must stay unreferenced before `Router::reap_
    /// unreferenced_backends` stops it. `None` (the default) disables
    /// this entirely -- unchanged behavior from before this field
    /// existed: a shared process, once spawned, otherwise only ever
    /// stops via an explicit `profiles/delete` or the whole daemon
    /// exiting. Deliberately independent of `idle_session_ttl`/
    /// `absolute_session_ttl` (which govern *session* retention, not
    /// *process* retention) -- a session can be reaped/closed while its
    /// backend process is still referenced by a sibling session under
    /// the same key, and conversely the last session under a key can
    /// close well before that key's own grace period elapses.
    pub connector_idle_shutdown_ttl: Option<std::time::Duration>,
    /// **`active_turn_deadline`.** How long a session's *current* turn
    /// may stay in-flight (`SessionEntry::in_flight != 0`, i.e. a
    /// `session/prompt`/`session/resume`/`session/load` round trip mid-
    /// flight against the backend) before `Router::cancel_stuck_turns`
    /// treats it as stuck: it sends the backend a best-effort
    /// `session/cancel` notification, then force-clears the session's
    /// in-flight bookkeeping so it stops being skipped by every future
    /// reaper pass (a genuinely still-running backend call already in
    /// flight is not itself interrupted -- there is no way to abort an
    /// in-progress `read_matching_response` await from outside the task
    /// running it -- but the *session* is no longer indefinitely
    /// reap-exempt because of it). `None` (the default) disables this
    /// entirely: unchanged, indefinite-skip behavior from before this
    /// field existed. Deliberately independent of `idle_session_ttl` --
    /// an in-flight session is by definition not idle, so it would
    /// never be selected by `SessionRegistry::reap_candidates` no matter
    /// how long the idle TTL is.
    pub active_turn_deadline: Option<std::time::Duration>,
    /// **`background_mode` (bg-mode `session/close` override).** When
    /// `true`, an explicit client `session/close` is deliberately *not*
    /// forwarded to the backend and does not evict the session from
    /// `SessionRegistry`/persistence -- the session, and whatever
    /// backend process is running it, stays fully alive so a later
    /// `session/resume`/`session/load`/`session/prompt` against the
    /// same gateway session id keeps working exactly as if the client
    /// had merely disconnected rather than explicitly closed. This
    /// mirrors `acpx-acp-bridge`'s existing "EOF never sends session/
    /// close" transport-level behavior one level up, at the JSON-RPC
    /// method itself, for callers (e.g. an editor closing a
    /// conversation tab) that call `session/close` explicitly rather
    /// than just dropping the connection. `false` (the default) keeps
    /// every pre-existing deployment's `session/close` semantics
    /// unchanged: a real, immediate close. A caller can override this
    /// per call regardless of the deployment-wide default via the
    /// additive, ACP-schema-external `_acpx.bg` extension field on the
    /// `session/close` request itself (`false`/`"off"` forces a real
    /// close even in background mode; `true`/`"on"` forces a
    /// background no-op even when this flag is `false`) -- see
    /// `router::take_background_override`. `session/delete` is
    /// deliberately unaffected either way: unlike `close`, `delete` is
    /// an unambiguous, explicit destroy-this-session's-data signal, and
    /// silently keeping a session alive under that call would be
    /// surprising for a caller that just asked to delete it.
    pub background_mode: bool,
    /// **`acpx-startup-recovery-unbounded`.** Bounds *startup session
    /// recovery* candidacy by recency -- a session record whose most
    /// recent known activity (`last_activity_at_unix_nanos`, falling
    /// back to `created_at_unix_nanos`) is older than this is never
    /// even attempted on the next startup recovery pass, regardless of
    /// its `status`/`recovery_method`. Deliberately independent of
    /// `idle_session_ttl` (which governs *live*, currently-registered
    /// session idling -- this governs whether a session that was never
    /// explicitly closed is still worth trying to *recover* after a
    /// process restart). Without this, `PersistenceStore::
    /// list_recoverable_sessions`'s query has no age bound at all: any
    /// session ever opened and never gracefully closed stays a
    /// recovery candidate forever. In practice a desktop client is
    /// almost always killed rather than shut down cleanly, so this
    /// list only ever grows -- confirmed live: a real, accumulated-
    /// over-~28-hours session database had 4367 such rows, and a
    /// single fresh startup recovery pass against it tried to recover
    /// every one of them, saturating the per-tenant session cap within
    /// seconds. `None` disables the bound entirely (unattenuated,
    /// pre-existing behavior) -- the default is `Some(24h)`, long
    /// enough to cover a real "closed the app overnight, reopened it
    /// the next day" continuity case, short enough that this list can
    /// never again silently grow into the thousands.
    pub startup_recovery_max_age: Option<std::time::Duration>,

    /// **`bound_new_registrations_per_session_list_call`.** Caps how many
    /// *newly*-registered sessions a single selector-bearing `session/list`
    /// call (`Router::dispatch_session_list_real`) may create via
    /// `translate_or_register_backend_session`. Sessions this tenant
    /// already owns keep translating for free and never count against
    /// this limit -- only genuinely first-seen backend sessions do.
    /// Without this, one `session/list` call proxies straight through to
    /// whatever the real backend reports and registers *every* session it
    /// names, with no bound beyond the tenant's total admission cap.
    /// Confirmed live: `claude-agent-acp` (a globally-shared `npx` tool)
    /// reports its entire session history across *every* project ever run
    /// on the machine, so a single `session/list` call -- reachable from
    /// panel-rust's own recoverable-sessions lookup whenever Settings is
    /// opened -- bulk-imported hundreds of unrelated cross-project
    /// sessions in one shot, saturating the shared per-tenant cap (this
    /// is the second, independent root cause of the same "512/512"
    /// symptom `startup_recovery_max_age` above was added for -- that fix
    /// alone did not stop this). `None` disables the bound entirely
    /// (unattenuated, pre-existing behavior); the default, `Some(50)`, is
    /// generous for legitimate multi-project/many-thread use while
    /// staying far below the total admission cap, so no single discovery
    /// call can come close to exhausting the shared budget other
    /// tenants/sessions depend on.
    pub max_new_sessions_per_list_call: Option<usize>,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            max_sessions_total: 128,
            max_sessions_per_tenant: 16,
            idle_session_ttl: std::time::Duration::from_secs(30 * 60),
            unbound_bridge_session_ttl: std::time::Duration::from_secs(5 * 60),
            absolute_session_ttl: None,
            max_pinned_sessions_per_tenant: None,
            connector_idle_shutdown_ttl: None,
            active_turn_deadline: None,
            background_mode: false,
            startup_recovery_max_age: Some(std::time::Duration::from_secs(24 * 60 * 60)),
            max_new_sessions_per_list_call: Some(50),
        }
    }
}

impl LifecycleConfig {
    pub fn validate(&self) -> Result<(), LifecycleConfigError> {
        if self.max_sessions_total == 0 {
            return Err(LifecycleConfigError::ZeroLimit("max_sessions_total"));
        }
        if self.max_sessions_per_tenant == 0 {
            return Err(LifecycleConfigError::ZeroLimit("max_sessions_per_tenant"));
        }
        if self.idle_session_ttl.is_zero() {
            return Err(LifecycleConfigError::ZeroDuration("idle_session_ttl"));
        }
        if self.unbound_bridge_session_ttl.is_zero() {
            return Err(LifecycleConfigError::ZeroDuration(
                "unbound_bridge_session_ttl",
            ));
        }
        if self.absolute_session_ttl.is_some_and(|ttl| ttl.is_zero()) {
            return Err(LifecycleConfigError::ZeroDuration("absolute_session_ttl"));
        }
        if self.max_pinned_sessions_per_tenant == Some(0) {
            return Err(LifecycleConfigError::ZeroLimit(
                "max_pinned_sessions_per_tenant",
            ));
        }
        if self
            .connector_idle_shutdown_ttl
            .is_some_and(|ttl| ttl.is_zero())
        {
            return Err(LifecycleConfigError::ZeroDuration(
                "connector_idle_shutdown_ttl",
            ));
        }
        if self.active_turn_deadline.is_some_and(|ttl| ttl.is_zero()) {
            return Err(LifecycleConfigError::ZeroDuration("active_turn_deadline"));
        }
        if self
            .startup_recovery_max_age
            .is_some_and(|ttl| ttl.is_zero())
        {
            return Err(LifecycleConfigError::ZeroDuration(
                "startup_recovery_max_age",
            ));
        }
        if self.max_new_sessions_per_list_call == Some(0) {
            return Err(LifecycleConfigError::ZeroLimit(
                "max_new_sessions_per_list_call",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LifecycleConfigError {
    #[error("lifecycle limit {0} must be greater than zero")]
    ZeroLimit(&'static str),
    #[error("lifecycle duration {0} must be greater than zero")]
    ZeroDuration(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe_for_a_shared_daemon() {
        assert_eq!(LifecycleConfig::default().max_sessions_total, 128);
        assert_eq!(LifecycleConfig::default().max_sessions_per_tenant, 16);
        assert_eq!(
            LifecycleConfig::default().idle_session_ttl,
            std::time::Duration::from_secs(30 * 60)
        );
        assert_eq!(
            LifecycleConfig::default().unbound_bridge_session_ttl,
            std::time::Duration::from_secs(5 * 60)
        );
    }

    #[test]
    fn zero_limits_are_rejected() {
        assert!(matches!(
            LifecycleConfig {
                max_sessions_total: 0,
                max_sessions_per_tenant: 1,
                ..Default::default()
            }
            .validate(),
            Err(LifecycleConfigError::ZeroLimit("max_sessions_total"))
        ));
    }
}
