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
    /// Fills-per-quotes numerator (fills_count is the numerator; quotes_sent the denominator).
    pub fill_rate_numerator: AtomicU64,
    /// Last cancel latency in microseconds.
    pub cancel_latency_us: AtomicU64,
    /// Last heartbeat age in milliseconds.
    pub heartbeat_age_ms: AtomicU64,
    /// Rate-limiter throttle event count.
    pub throttle_count: AtomicU64,
    /// Cumulative slippage across all fills.
    pub slippage_total: RwLock<Decimal>,
    /// Average quote staleness in milliseconds.
    pub quote_age_ms: AtomicU64,
    /// Number of quote replacement (cancel+replace) actions.
    pub quote_replacements: AtomicU64,
    /// Number of forced unwind orders submitted.
    pub forced_unwinds: AtomicU64,
    /// Number of hard-stop unwind failure events.
    pub unwind_hardstop_failures: AtomicU64,
    /// Number of scalper markets selected on the latest tick.
    pub scalper_markets_selected: AtomicU64,
    /// Cumulative 5-second markout in ticks.
    pub markout_5s_ticks_total: RwLock<Decimal>,
    /// Cumulative 30-second markout in ticks.
    pub markout_30s_ticks_total: RwLock<Decimal>,
}

impl Metrics {
    #[allow(dead_code)]
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
            fill_rate_numerator: AtomicU64::new(0),
            cancel_latency_us: AtomicU64::new(0),
            heartbeat_age_ms: AtomicU64::new(0),
            throttle_count: AtomicU64::new(0),
            slippage_total: RwLock::new(Decimal::ZERO),
            quote_age_ms: AtomicU64::new(0),
            quote_replacements: AtomicU64::new(0),
            forced_unwinds: AtomicU64::new(0),
            unwind_hardstop_failures: AtomicU64::new(0),
            scalper_markets_selected: AtomicU64::new(0),
            markout_5s_ticks_total: RwLock::new(Decimal::ZERO),
            markout_30s_ticks_total: RwLock::new(Decimal::ZERO),
        })
    }

    #[allow(dead_code)]
    pub fn inc_quotes_sent(&self) {
        self.quotes_sent.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn inc_quotes_cancelled(&self) {
        self.quotes_cancelled.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn inc_fills(&self) {
        self.fills_count.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn inc_cancel_failures(&self) {
        self.cancel_failures.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn inc_risk_rejections(&self) {
        self.risk_rejections.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn inc_ws_reconnects(&self) {
        self.ws_reconnects.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn inc_llm_calls(&self) {
        self.llm_calls.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn inc_llm_timeouts(&self) {
        self.llm_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn update_pnl(&self, pnl: Decimal) {
        *self.daily_pnl.write() = pnl;
    }

    #[allow(dead_code)]
    pub fn add_rebate(&self, amount: Decimal) {
        *self.rebate_accrual.write() += amount;
    }

    #[allow(dead_code)]
    pub fn inc_fill_rate_numerator(&self) {
        self.fill_rate_numerator.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn update_cancel_latency_us(&self, us: u64) {
        self.cancel_latency_us.store(us, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn update_heartbeat_age_ms(&self, ms: u64) {
        self.heartbeat_age_ms.store(ms, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn inc_throttle_count(&self) {
        self.throttle_count.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn add_slippage(&self, amount: Decimal) {
        *self.slippage_total.write() += amount;
    }

    #[allow(dead_code)]
    pub fn update_quote_age_ms(&self, ms: u64) {
        self.quote_age_ms.store(ms, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn inc_quote_replacements(&self) {
        self.quote_replacements.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn inc_forced_unwinds(&self) {
        self.forced_unwinds.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn inc_unwind_hardstop_failures(&self) {
        self.unwind_hardstop_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn update_scalper_markets_selected(&self, n: u64) {
        self.scalper_markets_selected.store(n, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn add_markout_5s_ticks(&self, ticks: Decimal) {
        *self.markout_5s_ticks_total.write() += ticks;
    }

    #[allow(dead_code)]
    pub fn add_markout_30s_ticks(&self, ticks: Decimal) {
        *self.markout_30s_ticks_total.write() += ticks;
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
            fill_rate_numerator: self.fill_rate_numerator.load(Ordering::Relaxed),
            cancel_latency_us: self.cancel_latency_us.load(Ordering::Relaxed),
            heartbeat_age_ms: self.heartbeat_age_ms.load(Ordering::Relaxed),
            throttle_count: self.throttle_count.load(Ordering::Relaxed),
            slippage_total: *self.slippage_total.read(),
            quote_age_ms: self.quote_age_ms.load(Ordering::Relaxed),
            quote_replacements: self.quote_replacements.load(Ordering::Relaxed),
            forced_unwinds: self.forced_unwinds.load(Ordering::Relaxed),
            unwind_hardstop_failures: self.unwind_hardstop_failures.load(Ordering::Relaxed),
            scalper_markets_selected: self.scalper_markets_selected.load(Ordering::Relaxed),
            markout_5s_ticks_total: *self.markout_5s_ticks_total.read(),
            markout_30s_ticks_total: *self.markout_30s_ticks_total.read(),
        }
    }

    /// Log a periodic metrics summary.
    #[allow(dead_code)]
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
            fill_rate_num = s.fill_rate_numerator,
            cancel_latency_us = s.cancel_latency_us,
            heartbeat_age_ms = s.heartbeat_age_ms,
            throttle_count = s.throttle_count,
            slippage = %s.slippage_total,
            quote_age_ms = s.quote_age_ms,
            quote_replacements = s.quote_replacements,
            forced_unwinds = s.forced_unwinds,
            unwind_hardstop_failures = s.unwind_hardstop_failures,
            scalper_markets_selected = s.scalper_markets_selected,
            markout_5s_ticks_total = %s.markout_5s_ticks_total,
            markout_30s_ticks_total = %s.markout_30s_ticks_total,
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
        self.fill_rate_numerator.store(0, Ordering::Relaxed);
        self.cancel_latency_us.store(0, Ordering::Relaxed);
        self.heartbeat_age_ms.store(0, Ordering::Relaxed);
        self.throttle_count.store(0, Ordering::Relaxed);
        self.quote_age_ms.store(0, Ordering::Relaxed);
        self.quote_replacements.store(0, Ordering::Relaxed);
        self.forced_unwinds.store(0, Ordering::Relaxed);
        self.unwind_hardstop_failures.store(0, Ordering::Relaxed);
        self.scalper_markets_selected.store(0, Ordering::Relaxed);
        *self.daily_pnl.write() = Decimal::ZERO;
        *self.rebate_accrual.write() = Decimal::ZERO;
        *self.slippage_total.write() = Decimal::ZERO;
        *self.markout_5s_ticks_total.write() = Decimal::ZERO;
        *self.markout_30s_ticks_total.write() = Decimal::ZERO;
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
    pub fill_rate_numerator: u64,
    pub cancel_latency_us: u64,
    pub heartbeat_age_ms: u64,
    pub throttle_count: u64,
    pub slippage_total: Decimal,
    pub quote_age_ms: u64,
    pub quote_replacements: u64,
    pub forced_unwinds: u64,
    pub unwind_hardstop_failures: u64,
    pub scalper_markets_selected: u64,
    pub markout_5s_ticks_total: Decimal,
    pub markout_30s_ticks_total: Decimal,
}

/// Spawn periodic metrics logging.
#[allow(dead_code)]
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

    #[test]
    fn fill_rate_numerator_tracking() {
        let m = Metrics::new();
        m.inc_fill_rate_numerator();
        m.inc_fill_rate_numerator();
        m.inc_fill_rate_numerator();
        assert_eq!(m.snapshot().fill_rate_numerator, 3);
    }

    #[test]
    fn cancel_latency_tracking() {
        let m = Metrics::new();
        m.update_cancel_latency_us(1500);
        assert_eq!(m.snapshot().cancel_latency_us, 1500);
        m.update_cancel_latency_us(800);
        assert_eq!(m.snapshot().cancel_latency_us, 800);
    }

    #[test]
    fn heartbeat_age_tracking() {
        let m = Metrics::new();
        m.update_heartbeat_age_ms(250);
        assert_eq!(m.snapshot().heartbeat_age_ms, 250);
    }

    #[test]
    fn throttle_count_tracking() {
        let m = Metrics::new();
        m.inc_throttle_count();
        m.inc_throttle_count();
        assert_eq!(m.snapshot().throttle_count, 2);
    }

    #[test]
    fn slippage_total_tracking() {
        let m = Metrics::new();
        m.add_slippage(dec!(0.5));
        m.add_slippage(dec!(1.25));
        assert_eq!(m.snapshot().slippage_total, dec!(1.75));
    }

    #[test]
    fn quote_age_tracking() {
        let m = Metrics::new();
        m.update_quote_age_ms(120);
        assert_eq!(m.snapshot().quote_age_ms, 120);
    }

    #[test]
    fn reset_daily_clears_new_metrics() {
        let m = Metrics::new();
        m.inc_fill_rate_numerator();
        m.update_cancel_latency_us(500);
        m.update_heartbeat_age_ms(100);
        m.inc_throttle_count();
        m.add_slippage(dec!(1.0));
        m.update_quote_age_ms(200);
        m.inc_quote_replacements();
        m.inc_forced_unwinds();
        m.inc_unwind_hardstop_failures();
        m.update_scalper_markets_selected(3);
        m.add_markout_5s_ticks(dec!(1.5));
        m.add_markout_30s_ticks(dec!(-0.5));

        m.reset_daily();
        let s = m.snapshot();
        assert_eq!(s.fill_rate_numerator, 0);
        assert_eq!(s.cancel_latency_us, 0);
        assert_eq!(s.heartbeat_age_ms, 0);
        assert_eq!(s.throttle_count, 0);
        assert_eq!(s.slippage_total, Decimal::ZERO);
        assert_eq!(s.quote_age_ms, 0);
        assert_eq!(s.quote_replacements, 0);
        assert_eq!(s.forced_unwinds, 0);
        assert_eq!(s.unwind_hardstop_failures, 0);
        assert_eq!(s.scalper_markets_selected, 0);
        assert_eq!(s.markout_5s_ticks_total, Decimal::ZERO);
        assert_eq!(s.markout_30s_ticks_total, Decimal::ZERO);
    }
}
