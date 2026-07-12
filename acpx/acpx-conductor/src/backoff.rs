//! Pure exponential-backoff delay calculation for conductor respawn
//! attempts. Deliberately free of any process/Supervisor state so it's
//! trivially unit-testable in isolation; `supervisor.rs` wires the
//! stateful consecutive-failure tracking (per agent id, last-attempt
//! timestamps) around this pure function.

use std::time::Duration;

/// Delay before the first respawn attempt after a crash.
pub const BASE_DELAY: Duration = Duration::from_millis(500);

/// Upper bound on the respawn delay, regardless of how many consecutive
/// failures have accumulated.
pub const MAX_DELAY: Duration = Duration::from_secs(30);

/// How long a respawned process must stay alive before its
/// consecutive-failure count is reset back to zero.
pub const STABLE_AFTER: Duration = Duration::from_secs(10);

/// Delay to wait before the next respawn attempt, given `consecutive_failures`
/// observed so far for an agent (0 = no prior failures yet, spawn
/// immediately). Doubles per failure starting from `BASE_DELAY`, capped at
/// `MAX_DELAY`.
pub fn next_delay(consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return Duration::ZERO;
    }
    // consecutive_failures=1 -> BASE_DELAY, =2 -> 2*BASE_DELAY, etc.
    let exponent = consecutive_failures - 1;
    let multiplier = 1u64.checked_shl(exponent).unwrap_or(u64::MAX);
    let millis = (BASE_DELAY.as_millis() as u64).saturating_mul(multiplier);
    Duration::from_millis(millis).min(MAX_DELAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_failures_means_no_delay() {
        assert_eq!(next_delay(0), Duration::ZERO);
    }

    #[test]
    fn delay_doubles_per_failure() {
        assert_eq!(next_delay(1), Duration::from_millis(500));
        assert_eq!(next_delay(2), Duration::from_millis(1000));
        assert_eq!(next_delay(3), Duration::from_millis(2000));
        assert_eq!(next_delay(4), Duration::from_millis(4000));
    }

    #[test]
    fn delay_caps_at_max() {
        assert_eq!(next_delay(20), MAX_DELAY);
        assert_eq!(next_delay(1000), MAX_DELAY);
    }
}
