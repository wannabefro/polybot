use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Simple token-bucket rate limiter.
///
/// Capped at 70% of published API limits per the plan.
#[derive(Debug)]
pub struct RateLimiter {
    inner: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64, // tokens per second
    last_refill: Instant,
}

impl RateLimiter {
    /// Create a rate limiter.
    /// `max_per_sec` is the published limit; we apply 70% factor.
    pub fn new(max_per_sec: f64) -> Arc<Self> {
        let effective = max_per_sec * 0.7;
        Arc::new(Self {
            inner: Mutex::new(Bucket {
                tokens: effective,
                max_tokens: effective,
                refill_rate: effective,
                last_refill: Instant::now(),
            }),
        })
    }

    /// Try to acquire one token. Returns Ok(()) or Err with wait duration.
    pub fn try_acquire(&self) -> Result<(), Duration> {
        let mut bucket = self.inner.lock();
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * bucket.refill_rate).min(bucket.max_tokens);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let wait = (1.0 - bucket.tokens) / bucket.refill_rate;
            Err(Duration::from_secs_f64(wait))
        }
    }

    /// Acquire, blocking if necessary.
    pub async fn acquire(&self) {
        loop {
            match self.try_acquire() {
                Ok(()) => return,
                Err(wait) => tokio::time::sleep(wait).await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_tokens_available() {
        let rl = RateLimiter::new(10.0); // 7 effective
        assert!(rl.try_acquire().is_ok());
    }

    #[test]
    fn exhausts_bucket() {
        let rl = RateLimiter::new(2.0); // 1.4 effective
        assert!(rl.try_acquire().is_ok()); // 1.4 → 0.4
        assert!(rl.try_acquire().is_err()); // 0.4 < 1.0, no tokens
    }
}
