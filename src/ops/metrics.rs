use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;
use rust_decimal::Decimal;
use serde::Serialize;
use tracing::info;

/// Operational metrics for monitoring and alerting.
#[derive(Debug)]
pub struct Metrics {
    pub quotes_sent: AtomicU64,
    pub quotes_cancelled: AtomicU64,
    pub fills_count: AtomicU64,
    pub cancel_failures: AtomicU64,
    pub risk_rejections: AtomicU64,
    pub ws_reconnects: AtomicU64,
    pub llm_calls: AtomicU64,
    pub llm_timeouts: AtomicU64,
    pub daily_pnl: RwLock<Decimal>,
    pub rebate_accrual: RwLock<Decimal>,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            quotes_sent: AtomicU64::new(0),
            quotes_cancelled: AtomicU64::new(0),
            fills_count: AtomicU64::new(0),
            cancel_failures: AtomicU64::new(0),
            risk_rejections: AtomicU64::new(0),
            ws_reconnects: AtomicU64::new(0),
            llm_calls: AtomicU64::new(0),
            llm_timeouts: AtomicU64::new(0),
            daily_pnl: RwLock::new(Decimal::ZERO),
            rebate_accrual: RwLock::new(Decimal::ZERO),
        })
    }

    pub fn inc_quotes_sent(&self) {
        self.quotes_sent.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_quotes_cancelled(&self) {
        self.quotes_cancelled.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_fills(&self) {
        self.fills_count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_cancel_failures(&self) {
        self.cancel_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_risk_rejections(&self) {
        self.risk_rejections.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_ws_reconnects(&self) {
        self.ws_reconnects.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_llm_calls(&self) {
        self.llm_calls.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_llm_timeouts(&self) {
        self.llm_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    pub fn update_pnl(&self, pnl: Decimal) {
        *self.daily_pnl.write() = pnl;
    }

    pub fn add_rebate(&self, amount: Decimal) {
        *self.rebate_accrual.write() += amount;
    }

    /// Snapshot current metrics for structured logging.
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            quotes_sent: self.quotes_sent.load(Ordering::Relaxed),
            quotes_cancelled: self.quotes_cancelled.load(Ordering::Relaxed),
            fills_count: self.fills_count.load(Ordering::Relaxed),
            cancel_failures: self.cancel_failures.load(Ordering::Relaxed),
            risk_rejections: self.risk_rejections.load(Ordering::Relaxed),
            ws_reconnects: self.ws_reconnects.load(Ordering::Relaxed),
            llm_calls: self.llm_calls.load(Ordering::Relaxed),
            llm_timeouts: self.llm_timeouts.load(Ordering::Relaxed),
            daily_pnl: *self.daily_pnl.read(),
            rebate_accrual: *self.rebate_accrual.read(),
        }
    }

    /// Log a periodic metrics summary.
    pub fn log_summary(&self) {
        let s = self.snapshot();
        info!(
            quotes_sent = s.quotes_sent,
            fills = s.fills_count,
            cancel_failures = s.cancel_failures,
            risk_rejections = s.risk_rejections,
            ws_reconnects = s.ws_reconnects,
            pnl = %s.daily_pnl,
            rebate = %s.rebate_accrual,
            "metrics: summary"
        );
    }

    /// Reset daily counters.
    pub fn reset_daily(&self) {
        self.quotes_sent.store(0, Ordering::Relaxed);
        self.quotes_cancelled.store(0, Ordering::Relaxed);
        self.fills_count.store(0, Ordering::Relaxed);
        self.cancel_failures.store(0, Ordering::Relaxed);
        self.risk_rejections.store(0, Ordering::Relaxed);
        *self.daily_pnl.write() = Decimal::ZERO;
        *self.rebate_accrual.write() = Decimal::ZERO;
    }
}

/// Serializable snapshot for JSON logging.
#[derive(Debug, Clone, Serialize)]
pub struct MetricsSnapshot {
    pub quotes_sent: u64,
    pub quotes_cancelled: u64,
    pub fills_count: u64,
    pub cancel_failures: u64,
    pub risk_rejections: u64,
    pub ws_reconnects: u64,
    pub llm_calls: u64,
    pub llm_timeouts: u64,
    pub daily_pnl: Decimal,
    pub rebate_accrual: Decimal,
}

/// Spawn periodic metrics logging.
pub fn spawn_logger(
    metrics: Arc<Metrics>,
    interval: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            metrics.log_summary();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn initial_counters_are_zero() {
        let m = Metrics::new();
        let s = m.snapshot();
        assert_eq!(s.quotes_sent, 0);
        assert_eq!(s.fills_count, 0);
        assert_eq!(s.cancel_failures, 0);
        assert_eq!(s.daily_pnl, Decimal::ZERO);
    }

    #[test]
    fn increment_counters() {
        let m = Metrics::new();
        m.inc_quotes_sent();
        m.inc_quotes_sent();
        m.inc_fills();
        m.inc_cancel_failures();
        m.inc_risk_rejections();
        m.inc_ws_reconnects();

        let s = m.snapshot();
        assert_eq!(s.quotes_sent, 2);
        assert_eq!(s.fills_count, 1);
        assert_eq!(s.cancel_failures, 1);
        assert_eq!(s.risk_rejections, 1);
        assert_eq!(s.ws_reconnects, 1);
    }

    #[test]
    fn pnl_and_rebate_tracking() {
        let m = Metrics::new();
        m.update_pnl(dec!(-15.50));
        m.add_rebate(dec!(2.30));
        m.add_rebate(dec!(1.10));

        let s = m.snapshot();
        assert_eq!(s.daily_pnl, dec!(-15.50));
        assert_eq!(s.rebate_accrual, dec!(3.40));
    }

    #[test]
    fn reset_daily_clears_counters() {
        let m = Metrics::new();
        m.inc_quotes_sent();
        m.inc_fills();
        m.update_pnl(dec!(100));
        m.add_rebate(dec!(5));

        m.reset_daily();
        let s = m.snapshot();
        assert_eq!(s.quotes_sent, 0);
        assert_eq!(s.fills_count, 0);
        assert_eq!(s.daily_pnl, Decimal::ZERO);
        assert_eq!(s.rebate_accrual, Decimal::ZERO);
    }

    #[test]
    fn snapshot_serializes_to_json() {
        let m = Metrics::new();
        m.inc_quotes_sent();
        m.update_pnl(dec!(42.50));

        let s = m.snapshot();
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"quotes_sent\":1"));
        assert!(json.contains("\"daily_pnl\":\"42.50\""));
    }

    #[test]
    fn concurrent_increments() {
        let m = Metrics::new();
        let m2 = Arc::clone(&m);

        // Simulate concurrent access
        std::thread::scope(|s| {
            s.spawn(|| {
                for _ in 0..1000 {
                    m.inc_quotes_sent();
                }
            });
            s.spawn(|| {
                for _ in 0..1000 {
                    m2.inc_quotes_sent();
                }
            });
        });

        assert_eq!(m.snapshot().quotes_sent, 2000);
    }

    #[test]
    fn ws_reconnects_and_llm_counters() {
        let m = Metrics::new();
        m.inc_llm_calls();
        m.inc_llm_calls();
        m.inc_llm_timeouts();

        let s = m.snapshot();
        assert_eq!(s.llm_calls, 2);
        assert_eq!(s.llm_timeouts, 1);
    }
}
