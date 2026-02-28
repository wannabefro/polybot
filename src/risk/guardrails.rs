use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;
use rust_decimal::Decimal;
use tracing::{error, warn};

use crate::config::Config;
use crate::order::pipeline::OrderIntent;

/// Pre-trade risk check results.
#[derive(Debug, Clone)]
pub enum RiskVerdict {
    Approved,
    Rejected(String),
}

/// Tracks positions and enforces risk limits.
#[derive(Debug)]
pub struct RiskEngine {
    config: Config,
    /// Per-market notional exposure (condition_id → USDC notional).
    market_exposure: RwLock<HashMap<String, Decimal>>,
    /// Daily realized P&L tracker.
    daily_pnl: RwLock<Decimal>,
    /// Emergency halt flag.
    halted: AtomicBool,
}

impl RiskEngine {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            market_exposure: RwLock::new(HashMap::new()),
            daily_pnl: RwLock::new(Decimal::ZERO),
            halted: AtomicBool::new(false),
        })
    }

    /// Check an order intent against all risk limits.
    pub fn check(&self, condition_id: &str, intent: &OrderIntent) -> RiskVerdict {
        if self.halted.load(Ordering::Relaxed) {
            return RiskVerdict::Rejected("risk engine halted".into());
        }

        let notional = intent.price * intent.size;

        // 1. Per-market notional cap (2% NAV default)
        let max_per_market = self.config.nav_limit(self.config.max_notional_per_market);
        let current = self
            .market_exposure
            .read()
            .get(condition_id)
            .copied()
            .unwrap_or(Decimal::ZERO);
        if current + notional > Decimal::from_f64_retain(max_per_market).unwrap_or(Decimal::MAX) {
            return RiskVerdict::Rejected(format!(
                "per-market limit: {current} + {notional} > {max_per_market}"
            ));
        }

        // 2. Gross exposure cap (25% NAV default)
        let max_gross = self.config.nav_limit(self.config.max_gross_exposure);
        let gross: Decimal = self.market_exposure.read().values().sum();
        if gross + notional > Decimal::from_f64_retain(max_gross).unwrap_or(Decimal::MAX) {
            return RiskVerdict::Rejected(format!(
                "gross exposure limit: {gross} + {notional} > {max_gross}"
            ));
        }

        // 3. Daily loss stop (3% NAV default)
        let max_loss = self.config.nav_limit(self.config.daily_loss_stop);
        let pnl = *self.daily_pnl.read();
        if pnl < Decimal::from_f64_retain(-max_loss).unwrap_or(Decimal::MIN) {
            return RiskVerdict::Rejected(format!("daily loss stop: P&L {pnl} < -{max_loss}"));
        }

        RiskVerdict::Approved
    }

    /// Record a fill (update exposure tracking).
    pub fn record_fill(&self, condition_id: &str, notional: Decimal) {
        let mut map = self.market_exposure.write();
        let entry = map.entry(condition_id.to_string()).or_insert(Decimal::ZERO);
        *entry += notional;
    }

    /// Record realized P&L.
    pub fn record_pnl(&self, amount: Decimal) {
        *self.daily_pnl.write() += amount;
    }

    /// Trigger emergency halt.
    pub fn halt(&self, reason: &str) {
        error!(reason, "risk: HALT triggered");
        self.halted.store(true, Ordering::Relaxed);
    }

    /// Check if engine is halted.
    pub fn is_halted(&self) -> bool {
        self.halted.load(Ordering::Relaxed)
    }

    /// Reset daily counters (call at UTC midnight or start of session).
    pub fn reset_daily(&self) {
        *self.daily_pnl.write() = Decimal::ZERO;
        warn!("risk: daily P&L counter reset");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::test_config;
    use polymarket_client_sdk::clob::types::{OrderType, Side};

    fn test_intent(price: f64, size: f64) -> OrderIntent {
        OrderIntent {
            token_id: "12345".into(),
            side: Side::Buy,
            price: Decimal::from_f64_retain(price).unwrap(),
            size: Decimal::from_f64_retain(size).unwrap(),
            order_type: OrderType::GTC,
            post_only: true,
        }
    }

    #[test]
    fn approved_within_limits() {
        let engine = RiskEngine::new(test_config());
        let verdict = engine.check("cond1", &test_intent(0.50, 100.0));
        assert!(matches!(verdict, RiskVerdict::Approved));
    }

    #[test]
    fn rejected_per_market_limit() {
        let engine = RiskEngine::new(test_config());
        engine.record_fill("cond1", Decimal::from(190));
        let verdict = engine.check("cond1", &test_intent(0.50, 10.0));
        assert!(matches!(verdict, RiskVerdict::Approved));
        let verdict = engine.check("cond1", &test_intent(0.50, 30.0));
        assert!(matches!(verdict, RiskVerdict::Rejected(_)));
    }

    #[test]
    fn rejected_when_halted() {
        let engine = RiskEngine::new(test_config());
        engine.halt("test halt");
        let verdict = engine.check("cond1", &test_intent(0.50, 1.0));
        assert!(matches!(verdict, RiskVerdict::Rejected(_)));
    }

    #[test]
    fn daily_loss_stop() {
        let engine = RiskEngine::new(test_config());
        engine.record_pnl(Decimal::from(-301));
        let verdict = engine.check("cond1", &test_intent(0.50, 1.0));
        assert!(matches!(verdict, RiskVerdict::Rejected(_)));
    }

    #[test]
    fn gross_exposure_limit() {
        let engine = RiskEngine::new(test_config());
        // Fill 12 markets to 200 each = 2400, under 2500 limit
        for i in 0..12 {
            engine.record_fill(&format!("cond{i}"), Decimal::from(200));
        }
        // 13th market with 200 would be 2600 > 2500
        let verdict = engine.check("cond_new", &test_intent(1.0, 200.0));
        assert!(matches!(verdict, RiskVerdict::Rejected(_)));
    }

    #[test]
    fn separate_markets_have_separate_limits() {
        let engine = RiskEngine::new(test_config());
        engine.record_fill("cond1", Decimal::from(190));
        // cond2 has no exposure, should pass easily
        let verdict = engine.check("cond2", &test_intent(0.50, 100.0));
        assert!(matches!(verdict, RiskVerdict::Approved));
    }

    #[test]
    fn daily_pnl_accumulates() {
        let engine = RiskEngine::new(test_config());
        engine.record_pnl(Decimal::from(-100));
        engine.record_pnl(Decimal::from(-100));
        // -200 is still within -300 limit
        let verdict = engine.check("cond1", &test_intent(0.50, 1.0));
        assert!(matches!(verdict, RiskVerdict::Approved));

        engine.record_pnl(Decimal::from(-101));
        // -301 exceeds limit
        let verdict = engine.check("cond1", &test_intent(0.50, 1.0));
        assert!(matches!(verdict, RiskVerdict::Rejected(_)));
    }

    #[test]
    fn reset_daily_clears_pnl() {
        let engine = RiskEngine::new(test_config());
        engine.record_pnl(Decimal::from(-301));
        assert!(matches!(
            engine.check("cond1", &test_intent(0.50, 1.0)),
            RiskVerdict::Rejected(_)
        ));

        engine.reset_daily();
        assert!(matches!(
            engine.check("cond1", &test_intent(0.50, 1.0)),
            RiskVerdict::Approved
        ));
    }

    #[test]
    fn halt_and_check_is_halted() {
        let engine = RiskEngine::new(test_config());
        assert!(!engine.is_halted());
        engine.halt("test");
        assert!(engine.is_halted());
    }

    #[test]
    fn zero_notional_order_passes() {
        let engine = RiskEngine::new(test_config());
        let verdict = engine.check("cond1", &test_intent(0.0, 10.0));
        assert!(matches!(verdict, RiskVerdict::Approved));
    }
}
