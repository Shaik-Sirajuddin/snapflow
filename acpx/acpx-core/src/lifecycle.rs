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
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            max_sessions_total: 128,
            max_sessions_per_tenant: 16,
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
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LifecycleConfigError {
    #[error("lifecycle limit {0} must be greater than zero")]
    ZeroLimit(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe_for_a_shared_daemon() {
        assert_eq!(LifecycleConfig::default().max_sessions_total, 128);
        assert_eq!(LifecycleConfig::default().max_sessions_per_tenant, 16);
    }

    #[test]
    fn zero_limits_are_rejected() {
        assert!(matches!(
            LifecycleConfig {
                max_sessions_total: 0,
                max_sessions_per_tenant: 1,
            }
            .validate(),
            Err(LifecycleConfigError::ZeroLimit("max_sessions_total"))
        ));
    }
}
