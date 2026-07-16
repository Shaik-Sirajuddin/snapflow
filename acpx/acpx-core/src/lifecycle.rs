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
