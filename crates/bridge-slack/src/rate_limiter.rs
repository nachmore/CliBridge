use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// Rate limiter that enforces a minimum interval between operations.
/// Slack allows ~1 message per second per channel.
pub struct RateLimiter {
    min_interval: Duration,
    last_call: Mutex<Option<Instant>>,
}

impl RateLimiter {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last_call: Mutex::new(None),
        }
    }

    /// Wait until the rate limit allows the next call, then mark it as used.
    pub async fn acquire(&self) {
        let mut last = self.last_call.lock().await;
        if let Some(last_time) = *last {
            let elapsed = last_time.elapsed();
            if elapsed < self.min_interval {
                tokio::time::sleep(self.min_interval - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_rate_limiter_first_call_immediate() {
        let limiter = RateLimiter::new(Duration::from_millis(100));
        let start = Instant::now();
        limiter.acquire().await;
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn test_rate_limiter_enforces_interval() {
        let limiter = RateLimiter::new(Duration::from_millis(100));
        limiter.acquire().await;
        let start = Instant::now();
        limiter.acquire().await;
        // Tight lower bound — catches a regression where sleep is silently
        // skipped (e.g. due to a unit error or a saturating subtract bug).
        // Allow 10ms of timer slack; CI clocks can be jittery but not by
        // 90ms+.
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(90),
            "second acquire returned in {elapsed:?}, expected >= 90ms"
        );
    }

    #[tokio::test]
    async fn test_rate_limiter_third_call_also_waits() {
        // Confirms the limiter resets `last_call` on each acquire — without
        // that, a back-to-back burst after the first interval would all
        // pass through.
        let limiter = RateLimiter::new(Duration::from_millis(50));
        limiter.acquire().await;
        limiter.acquire().await;
        let start = Instant::now();
        limiter.acquire().await;
        assert!(
            start.elapsed() >= Duration::from_millis(40),
            "third acquire didn't wait its interval"
        );
    }

    #[tokio::test]
    async fn test_rate_limiter_no_wait_after_long_idle() {
        // A long pause between calls means the next one shouldn't sleep at
        // all — important for the bridge's tick-driven posting cadence
        // when the user is inactive.
        let limiter = RateLimiter::new(Duration::from_millis(50));
        limiter.acquire().await;
        tokio::time::sleep(Duration::from_millis(80)).await;
        let start = Instant::now();
        limiter.acquire().await;
        assert!(
            start.elapsed() < Duration::from_millis(20),
            "acquire blocked despite the interval already having elapsed"
        );
    }
}
