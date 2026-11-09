use std::time::Duration;

/// Restart tuning. Defaults match `backendSupervisionPolicy.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// Consecutive failed starts, with no readiness in between, before giving up.
    pub max_consecutive_failures: u32,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(10),
            max_consecutive_failures: 5,
        }
    }
}

/// Delay before restart `attempt` (1-based): `base · 2^(attempt-1)`, capped.
pub fn restart_delay(policy: &RestartPolicy, attempt: u32) -> Duration {
    let step = attempt.max(1) - 1;
    let factor = 1u32.checked_shl(step.min(16)).unwrap_or(u32::MAX);
    policy
        .base_delay
        .saturating_mul(factor)
        .min(policy.max_delay)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureResponse {
    Retry { delay: Duration, attempt: u32 },
    GiveUp { failures: u32 },
}

/// Counts consecutive failed starts. Cleared by readiness or a deliberate
/// start, never by a successful spawn: spawning nearly always succeeds, and
/// clearing on spawn would pin the backoff at its first step.
#[derive(Debug, Clone)]
pub struct SupervisionPolicy {
    policy: RestartPolicy,
    failures: u32,
    given_up: bool,
}

impl SupervisionPolicy {
    pub fn new(policy: RestartPolicy) -> Self {
        Self {
            policy,
            failures: 0,
            given_up: false,
        }
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.failures
    }

    pub fn has_given_up(&self) -> bool {
        self.given_up
    }

    /// A deliberate (user or lifecycle) start.
    pub fn reset(&mut self) {
        self.failures = 0;
        self.given_up = false;
    }

    pub fn record_ready(&mut self) {
        self.reset();
    }

    pub fn record_failure(&mut self) -> FailureResponse {
        if self.given_up {
            return FailureResponse::GiveUp {
                failures: self.failures,
            };
        }
        self.failures += 1;
        if self.failures >= self.policy.max_consecutive_failures {
            self.given_up = true;
            return FailureResponse::GiveUp {
                failures: self.failures,
            };
        }
        FailureResponse::Retry {
            delay: restart_delay(&self.policy, self.failures),
            attempt: self.failures,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_match_the_desktop_policy() {
        let policy = RestartPolicy::default();
        let delays: Vec<u128> = (1..=7)
            .map(|a| restart_delay(&policy, a).as_millis())
            .collect();
        assert_eq!(delays, [500, 1000, 2000, 4000, 8000, 10_000, 10_000]);
        assert_eq!(restart_delay(&policy, 0), policy.base_delay);
        assert_eq!(restart_delay(&policy, u32::MAX), policy.max_delay);
    }

    #[test]
    fn gives_up_after_five_and_readiness_clears_it() {
        let mut policy = SupervisionPolicy::new(RestartPolicy::default());
        for attempt in 1..=4 {
            assert!(matches!(
                policy.record_failure(),
                FailureResponse::Retry { attempt: a, .. } if a == attempt
            ));
        }
        assert_eq!(
            policy.record_failure(),
            FailureResponse::GiveUp { failures: 5 }
        );
        // Stays given up without counting further.
        assert_eq!(
            policy.record_failure(),
            FailureResponse::GiveUp { failures: 5 }
        );
        assert!(policy.has_given_up());
        policy.record_ready();
        assert_eq!(policy.consecutive_failures(), 0);
        assert!(matches!(
            policy.record_failure(),
            FailureResponse::Retry { attempt: 1, .. }
        ));
    }
}
