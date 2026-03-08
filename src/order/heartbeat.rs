// Heartbeat management is handled automatically by the SDK when the
// `heartbeats` feature is enabled. The SDK sends heartbeats at the
// configured interval (default 5s) and cancels all orders if a
// heartbeat fails.
//
// This module provides monitoring on top of the SDK's built-in heartbeat.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;

/// Heartbeat health monitor — tracks the last known heartbeat success.
#[derive(Debug)]
#[allow(dead_code)]
pub struct HeartbeatMonitor {
    last_success: RwLock<Instant>,
    interval: Duration,
    /// Rolling heartbeat ID (set by the server on each heartbeat response).
    heartbeat_id: RwLock<Option<String>>,
    /// Consecutive heartbeat failure counter.
    consecutive_failures: AtomicU32,
}

impl HeartbeatMonitor {
    pub fn new(interval: Duration) -> Arc<Self> {
        Arc::new(Self {
            last_success: RwLock::new(Instant::now()),
            interval,
            heartbeat_id: RwLock::new(None),
            consecutive_failures: AtomicU32::new(0),
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

    /// Set the rolling heartbeat ID (returned by server).
    #[allow(dead_code)]
    pub fn set_id(&self, id: String) {
        *self.heartbeat_id.write() = Some(id);
    }

    /// Get the current heartbeat ID.
    #[allow(dead_code)]
    pub fn id(&self) -> Option<String> {
        self.heartbeat_id.read().clone()
    }

    /// Record a heartbeat failure (increments consecutive failure count).
    #[allow(dead_code)]
    pub fn record_failure(&self) {
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Get consecutive failure count.
    #[allow(dead_code)]
    pub fn failure_count(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    /// Reset failure count (on successful heartbeat).
    #[allow(dead_code)]
    pub fn reset_failures(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
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

    #[test]
    fn set_and_get_heartbeat_id() {
        let mon = HeartbeatMonitor::new(Duration::from_secs(5));
        assert!(mon.id().is_none());
        mon.set_id("hb-12345".into());
        assert_eq!(mon.id(), Some("hb-12345".into()));
    }

    #[test]
    fn heartbeat_id_updates() {
        let mon = HeartbeatMonitor::new(Duration::from_secs(5));
        mon.set_id("hb-1".into());
        mon.set_id("hb-2".into());
        assert_eq!(mon.id(), Some("hb-2".into()));
    }

    #[test]
    fn record_failure_increments() {
        let mon = HeartbeatMonitor::new(Duration::from_secs(5));
        assert_eq!(mon.failure_count(), 0);
        mon.record_failure();
        mon.record_failure();
        assert_eq!(mon.failure_count(), 2);
    }

    #[test]
    fn reset_failures_clears_count() {
        let mon = HeartbeatMonitor::new(Duration::from_secs(5));
        mon.record_failure();
        mon.record_failure();
        mon.reset_failures();
        assert_eq!(mon.failure_count(), 0);
    }
}
