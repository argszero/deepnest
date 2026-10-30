//! How often, and how long, to wait before trying again.
//!
//! Retrying is a decision about **one class of failure** ([`LlmError::is_transient`]) taken
//! with a budget. A refusal or an over-long request is not in that class, so no amount of
//! patience fixes it and the loop must not spend any: the caller has to change the request
//! instead.

use std::time::Duration;

use crate::error::LlmError;

/// Attempts per request, the first one included.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 4;
/// Delay after the first failed attempt; each later wait doubles.
pub const DEFAULT_BASE_DELAY: Duration = Duration::from_secs(1);
/// Ceiling for a single wait, so a long backoff cannot outlast the user's patience.
pub const DEFAULT_MAX_DELAY: Duration = Duration::from_secs(16);

/// The retry budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    max_attempts: u32,
    base_delay: Duration,
    max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            base_delay: DEFAULT_BASE_DELAY,
            max_delay: DEFAULT_MAX_DELAY,
        }
    }
}

impl RetryPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    pub fn with_base_delay(mut self, base_delay: Duration) -> Self {
        self.base_delay = base_delay;
        self
    }

    pub fn with_max_delay(mut self, max_delay: Duration) -> Self {
        self.max_delay = max_delay;
        self
    }

    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// The wait after `attempt` failed, counting attempts from 1: base, 2×base, 4×base, …
    /// up to the ceiling.
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let doublings = attempt.saturating_sub(1).min(16);
        let scaled = self.base_delay.saturating_mul(1u32 << doublings);
        scaled.min(self.max_delay)
    }

    /// Whether another attempt is both allowed by the budget and warranted by the failure.
    pub fn may_retry(&self, attempt: u32, error: &LlmError) -> bool {
        attempt < self.max_attempts && error.is_transient()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_from_the_base_and_stop_at_the_ceiling() {
        let policy = RetryPolicy::new();
        assert_eq!(policy.delay_for(1), Duration::from_secs(1));
        assert_eq!(policy.delay_for(2), Duration::from_secs(2));
        assert_eq!(policy.delay_for(3), Duration::from_secs(4));
        assert_eq!(policy.delay_for(4), Duration::from_secs(8));
        assert_eq!(policy.delay_for(9), DEFAULT_MAX_DELAY);
    }

    #[test]
    fn the_budget_is_spent_by_attempt_number() {
        let policy = RetryPolicy::new().with_max_attempts(2);
        let transient = LlmError::classify(Some(503), "unavailable");
        assert!(policy.may_retry(1, &transient));
        assert!(!policy.may_retry(2, &transient));
    }

    #[test]
    fn a_refusal_spends_no_budget_at_all() {
        let policy = RetryPolicy::new();
        let refused = LlmError::classify(Some(400), "Content Exists Risk");
        assert!(!policy.may_retry(1, &refused));
    }

    #[test]
    fn an_overlong_request_is_not_retried_even_on_a_transient_status() {
        // A 413 from a proxy: retrying identical bytes can only fail identically.
        let policy = RetryPolicy::new();
        let too_big = LlmError::classify(Some(413), "body too large");
        assert!(!policy.may_retry(1, &too_big));
    }

    #[test]
    fn a_zero_attempt_budget_is_lifted_to_one() {
        assert_eq!(RetryPolicy::new().with_max_attempts(0).max_attempts(), 1);
    }
}
