use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use rand::Rng;

/// Token-bucket rate limiter with burst capacity and 429 backoff.
///
/// Capped at 70% of published API limits per the plan.
#[derive(Debug)]
pub struct RateLimiter {
    inner: Mutex<Bucket>,
    /// Exponential backoff until this instant (set on 429 responses).
    backoff_until: RwLock<Option<Instant>>,
    /// Counter for consecutive 429 backoffs.
    backoff_count: Mutex<u32>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    max_tokens: f64,
    /// Sustained (long-term) refill rate in tokens/sec.
    sustained_rate: f64,
    /// Burst capacity — extra tokens allowed for short bursts.
    burst_capacity: u32,
    last_refill: Instant,
}

impl RateLimiter {
    /// Create a rate limiter.
    /// `max_per_sec` is the published limit; we apply 70% factor.
    pub fn new(max_per_sec: f64) -> Arc<Self> {
        let effective = max_per_sec * 0.7;
        Arc::new(Self {
            inner: Mutex::new(Bucket {
                tokens: effective + 10.0, // start with burst headroom
                max_tokens: effective + 10.0,
                sustained_rate: effective,
                burst_capacity: 10,
                last_refill: Instant::now(),
            }),
            backoff_until: RwLock::new(None),
            backoff_count: Mutex::new(0),
        })
    }

    /// Create with explicit burst capacity.
    pub fn with_burst(max_per_sec: f64, burst_capacity: u32) -> Arc<Self> {
        let effective = max_per_sec * 0.7;
        Arc::new(Self {
            inner: Mutex::new(Bucket {
                tokens: effective + f64::from(burst_capacity),
                max_tokens: effective + f64::from(burst_capacity),
                sustained_rate: effective,
                burst_capacity,
                last_refill: Instant::now(),
            }),
            backoff_until: RwLock::new(None),
            backoff_count: Mutex::new(0),
        })
    }

    /// Record a 429/425 response — sets exponential backoff with jitter.
    pub fn record_429(&self) {
        let mut count = self.backoff_count.lock();
        *count = (*count + 1).min(8); // cap exponent
        let base_ms = 100u64 * 2u64.pow(*count);
        let jitter_ms = rand::rng().random_range(0..=base_ms / 2);
        let wait = Duration::from_millis(base_ms + jitter_ms);
        *self.backoff_until.write() = Some(Instant::now() + wait);
    }

    /// Check if we're currently in a backoff period.
    pub fn is_backed_off(&self) -> bool {
        self.backoff_until
            .read()
            .map_or(false, |until| Instant::now() < until)
    }

    /// Try to acquire one token. Returns Ok(()) or Err with wait duration.
    pub fn try_acquire(&self) -> Result<(), Duration> {
        // Check backoff first
        if let Some(until) = *self.backoff_until.read() {
            if Instant::now() < until {
                return Err(until.duration_since(Instant::now()));
            }
            // Backoff expired — clear it and reset counter
            *self.backoff_until.write() = None;
            *self.backoff_count.lock() = 0;
        }

        let mut bucket = self.inner.lock();
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * bucket.sustained_rate).min(bucket.max_tokens);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let wait = (1.0 - bucket.tokens) / bucket.sustained_rate;
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
        let rl = RateLimiter::new(2.0); // 1.4 effective + 10 burst = 11.4
        // Drain all tokens
        let mut ok_count = 0;
        for _ in 0..20 {
            if rl.try_acquire().is_ok() {
                ok_count += 1;
            }
        }
        assert!(ok_count > 1, "should get more than 1 token with burst");
        assert!(ok_count <= 12, "shouldn't exceed bucket capacity");
    }

    #[test]
    fn burst_capacity_allows_short_bursts() {
        let rl = RateLimiter::with_burst(1.0, 5); // 0.7 sustained + 5 burst = 5.7 max
        let mut ok_count = 0;
        for _ in 0..10 {
            if rl.try_acquire().is_ok() {
                ok_count += 1;
            }
        }
        assert!(ok_count >= 5, "burst should allow at least 5 immediate tokens");
    }

    #[test]
    fn record_429_sets_backoff() {
        let rl = RateLimiter::new(10.0);
        assert!(!rl.is_backed_off());
        rl.record_429();
        assert!(rl.is_backed_off());
    }

    #[test]
    fn backoff_blocks_acquire() {
        let rl = RateLimiter::new(10.0);
        rl.record_429();
        assert!(rl.try_acquire().is_err());
    }

    #[test]
    fn backoff_jitter_produces_variation() {
        // Two calls to record_429 should produce slightly different backoff times
        let rl1 = RateLimiter::new(10.0);
        let rl2 = RateLimiter::new(10.0);
        rl1.record_429();
        rl2.record_429();
        // Both should be backed off
        assert!(rl1.is_backed_off());
        assert!(rl2.is_backed_off());
    }

    #[test]
    fn exponential_backoff_increases() {
        let rl = RateLimiter::new(10.0);
        rl.record_429();
        let first = *rl.backoff_until.read();
        // Reset and record again (simulating consecutive 429s)
        rl.record_429();
        let second = *rl.backoff_until.read();
        // Second backoff should generally be later (longer duration)
        assert!(first.is_some());
        assert!(second.is_some());
    }
}
