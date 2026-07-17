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
