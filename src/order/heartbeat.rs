// Heartbeat management is handled automatically by the SDK when the
// `heartbeats` feature is enabled. The SDK sends heartbeats at the
// configured interval (default 5s) and cancels all orders if a
// heartbeat fails.
//
// This module provides monitoring on top of the SDK's built-in heartbeat.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use tracing::{info, warn};

use crate::ops::metrics::Metrics;

/// Heartbeat health monitor — tracks the last known heartbeat success.
#[derive(Debug)]
pub struct HeartbeatMonitor {
    last_success: RwLock<Instant>,
    interval: Duration,
}

impl HeartbeatMonitor {
    pub fn new(interval: Duration) -> Arc<Self> {
        Arc::new(Self {
            last_success: RwLock::new(Instant::now()),
            interval,
        })
    }

    /// Record a successful heartbeat.
    pub fn record_success(&self) {
        *self.last_success.write() = Instant::now();
    }

    /// How long since the last successful heartbeat.
    pub fn since_last(&self) -> Duration {
        self.last_success.read().elapsed()
    }

    /// True if we haven't had a heartbeat in 3× the interval.
    pub fn is_stale(&self) -> bool {
        self.since_last() > self.interval * 3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_monitor_is_not_stale() {
        let mon = HeartbeatMonitor::new(Duration::from_secs(5));
        assert!(!mon.is_stale());
        assert!(mon.since_last() < Duration::from_secs(1));
    }

    #[test]
    fn record_success_resets_timer() {
        let mon = HeartbeatMonitor::new(Duration::from_secs(5));
        // Artificially set old timestamp
        *mon.last_success.write() = Instant::now() - Duration::from_secs(20);
        assert!(mon.is_stale());

        mon.record_success();
        assert!(!mon.is_stale());
        assert!(mon.since_last() < Duration::from_secs(1));
    }

    #[test]
    fn stale_after_three_intervals() {
        let mon = HeartbeatMonitor::new(Duration::from_millis(10));
        *mon.last_success.write() = Instant::now() - Duration::from_millis(31);
        assert!(mon.is_stale());
    }

    #[test]
    fn not_stale_within_threshold() {
        let mon = HeartbeatMonitor::new(Duration::from_secs(5));
        *mon.last_success.write() = Instant::now() - Duration::from_secs(10);
        assert!(!mon.is_stale()); // 10s < 15s (3×5)
    }
}
