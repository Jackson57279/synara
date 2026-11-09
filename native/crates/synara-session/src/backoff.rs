use std::time::Duration;

/// Exponential backoff with full jitter in the upper half: the delay for
/// attempt `n` is uniform in `[base·2ⁿ/2, base·2ⁿ]`, capped at `max`.
#[derive(Debug, Clone)]
pub struct Backoff {
    pub base: Duration,
    pub max: Duration,
    attempt: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(Duration::from_millis(250), Duration::from_secs(10))
    }
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            attempt: 0,
        }
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// The ceiling for the current attempt, before jitter.
    pub fn ceiling(&self) -> Duration {
        let factor = 1u32.checked_shl(self.attempt.min(16)).unwrap_or(u32::MAX);
        self.base.saturating_mul(factor).min(self.max)
    }

    /// The delay to wait now; advances the attempt counter.
    pub fn next_delay(&mut self) -> Duration {
        let ceiling = self.ceiling();
        self.attempt = self.attempt.saturating_add(1);
        let low = ceiling / 2;
        let spread = ceiling.saturating_sub(low).as_millis() as u64;
        low + Duration::from_millis(rand::random_range(0..=spread))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_caps_and_resets() {
        let mut backoff = Backoff::new(Duration::from_millis(100), Duration::from_secs(1));
        let mut ceilings = Vec::new();
        for _ in 0..8 {
            let ceiling = backoff.ceiling();
            let delay = backoff.next_delay();
            assert!(
                delay <= ceiling && delay >= ceiling / 2,
                "{delay:?} vs {ceiling:?}"
            );
            ceilings.push(ceiling.as_millis());
        }
        assert_eq!(ceilings, [100, 200, 400, 800, 1000, 1000, 1000, 1000]);
        backoff.reset();
        assert_eq!(backoff.ceiling(), Duration::from_millis(100));
    }

    #[test]
    fn huge_attempt_counts_do_not_overflow() {
        let mut backoff = Backoff::default();
        for _ in 0..200 {
            backoff.next_delay();
        }
        assert_eq!(backoff.ceiling(), backoff.max);
    }
}
