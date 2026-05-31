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
        assert!(start.elapsed() >= Duration::from_millis(90));
    }
}
