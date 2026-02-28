use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;
use polymarket_client_sdk::clob::types::Side;
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
    /// Per-token directional exposure (token_id → signed size: +buy, -sell).
    token_inventory: RwLock<HashMap<String, Decimal>>,
    /// Daily realized P&L tracker.
    daily_pnl: RwLock<Decimal>,
    /// Emergency halt flag.
    halted: AtomicBool,
    /// Consecutive cancel failure counter.
    cancel_failures: AtomicU32,
}

const CANCEL_FAILURE_HALT_THRESHOLD: u32 = 3;

impl RiskEngine {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            market_exposure: RwLock::new(HashMap::new()),
            token_inventory: RwLock::new(HashMap::new()),
            daily_pnl: RwLock::new(Decimal::ZERO),
            halted: AtomicBool::new(false),
            cancel_failures: AtomicU32::new(0),
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

        // 4. One-sided inventory cap (1% NAV default)
        let max_inventory =
            Decimal::from_f64_retain(self.config.nav_limit(self.config.max_one_sided_inventory))
                .unwrap_or(Decimal::MAX);
        let current_inv = self
            .token_inventory
            .read()
            .get(&intent.token_id)
            .copied()
            .unwrap_or(Decimal::ZERO);
        let delta = match intent.side {
            Side::Buy => intent.size,
            Side::Sell => -intent.size,
            _ => Decimal::ZERO,
        };
        let new_inv = current_inv + delta;
        // Check absolute value of new inventory * mid price ≈ notional
        let inv_notional = new_inv.abs() * intent.price;
        if inv_notional > max_inventory {
            return RiskVerdict::Rejected(format!(
                "one-sided inventory: {inv_notional} > {max_inventory}"
            ));
        }

        RiskVerdict::Approved
    }

    /// Record a fill (update exposure + inventory tracking).
    pub fn record_fill(
        &self,
        condition_id: &str,
        token_id: &str,
        side: Side,
        size: Decimal,
        notional: Decimal,
    ) {
        {
            let mut map = self.market_exposure.write();
            let entry = map.entry(condition_id.to_string()).or_insert(Decimal::ZERO);
            *entry += notional;
        }
        {
            let mut inv = self.token_inventory.write();
            let entry = inv.entry(token_id.to_string()).or_insert(Decimal::ZERO);
            match side {
                Side::Buy => *entry += size,
                Side::Sell => *entry -= size,
                _ => {}
            }
        }
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

    /// Record a cancel failure. Returns true if halt threshold reached.
    pub fn record_cancel_failure(&self) -> bool {
        let count = self.cancel_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if count >= CANCEL_FAILURE_HALT_THRESHOLD {
            self.halt(&format!("consecutive cancel failures: {count}"));
            return true;
        }
        warn!(count, "risk: cancel failure recorded");
        false
    }

    /// Reset cancel failure counter (call after successful cancel).
    pub fn reset_cancel_failures(&self) {
        self.cancel_failures.store(0, Ordering::Relaxed);
    }

    /// Resume from halt (used by reset_daily).
    pub fn resume(&self) {
        self.halted.store(false, Ordering::Relaxed);
        warn!("risk: resumed from halt");
    }

    /// Reset daily counters (call at UTC midnight or start of session).
    pub fn reset_daily(&self) {
        *self.daily_pnl.write() = Decimal::ZERO;
        self.cancel_failures.store(0, Ordering::Relaxed);
        self.resume();
        warn!("risk: daily counters reset");
    }

    /// Get current inventory for a token.
    pub fn token_inventory(&self, token_id: &str) -> Decimal {
        self.token_inventory
            .read()
            .get(token_id)
            .copied()
            .unwrap_or(Decimal::ZERO)
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

    fn sell_intent(token: &str, price: f64, size: f64) -> OrderIntent {
        OrderIntent {
            token_id: token.into(),
            side: Side::Sell,
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
        // Record a fill that uses most of the per-market allowance (2% NAV = 200)
        // Use small size so inventory check doesn't fire first
        engine.record_fill("cond1", "12345", Side::Buy, Decimal::from(10), Decimal::from(190));
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
        for i in 0..12 {
            // Use different tokens so inventory doesn't accumulate on one
            engine.record_fill(&format!("cond{i}"), &format!("t{i}"), Side::Buy, Decimal::from(5), Decimal::from(200));
        }
        let verdict = engine.check("cond_new", &test_intent(1.0, 200.0));
        assert!(matches!(verdict, RiskVerdict::Rejected(_)));
    }

    #[test]
    fn separate_markets_have_separate_limits() {
        let engine = RiskEngine::new(test_config());
        // Use a different token so cond2 check on "12345" has clean inventory
        engine.record_fill("cond1", "other_token", Side::Buy, Decimal::from(5), Decimal::from(190));
        let verdict = engine.check("cond2", &test_intent(0.50, 100.0));
        assert!(matches!(verdict, RiskVerdict::Approved));
    }

    #[test]
    fn daily_pnl_accumulates() {
        let engine = RiskEngine::new(test_config());
        engine.record_pnl(Decimal::from(-100));
        engine.record_pnl(Decimal::from(-100));
        let verdict = engine.check("cond1", &test_intent(0.50, 1.0));
        assert!(matches!(verdict, RiskVerdict::Approved));

        engine.record_pnl(Decimal::from(-101));
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

    // ── New: one-sided inventory tests ──

    #[test]
    fn one_sided_inventory_buy_limit() {
        let engine = RiskEngine::new(test_config());
        // max_one_sided_inventory = 0.01 → 1% of 10000 = 100 USDC
        // Buy 200 shares at 0.50 = 100 notional inventory
        engine.record_fill("cond1", "token1", Side::Buy, Decimal::from(200), Decimal::from(100));
        // Another buy of 10 at 0.50 would push inventory to 105 > 100
        let intent = OrderIntent {
            token_id: "token1".into(),
            side: Side::Buy,
            price: Decimal::from_f64_retain(0.50).unwrap(),
            size: Decimal::from(10),
            order_type: OrderType::GTC,
            post_only: true,
        };
        let verdict = engine.check("cond1", &intent);
        assert!(matches!(verdict, RiskVerdict::Rejected(_)));
    }

    #[test]
    fn one_sided_inventory_sell_reduces() {
        let engine = RiskEngine::new(test_config());
        // Long 200 shares
        engine.record_fill("cond1", "token1", Side::Buy, Decimal::from(200), Decimal::from(100));
        // Selling reduces inventory — should pass
        let intent = sell_intent("token1", 0.50, 100.0);
        let verdict = engine.check("cond1", &intent);
        assert!(matches!(verdict, RiskVerdict::Approved));
    }

    #[test]
    fn one_sided_inventory_short_limit() {
        let engine = RiskEngine::new(test_config());
        // Short 200 shares at 0.50 = 100 notional
        engine.record_fill("cond1", "token1", Side::Sell, Decimal::from(200), Decimal::from(100));
        // More selling would push past limit
        let intent = sell_intent("token1", 0.50, 10.0);
        let verdict = engine.check("cond1", &intent);
        assert!(matches!(verdict, RiskVerdict::Rejected(_)));
    }

    // ── New: cancel failure halt tests ──

    #[test]
    fn cancel_failure_below_threshold() {
        let engine = RiskEngine::new(test_config());
        assert!(!engine.record_cancel_failure());
        assert!(!engine.record_cancel_failure());
        assert!(!engine.is_halted());
    }

    #[test]
    fn cancel_failure_at_threshold_halts() {
        let engine = RiskEngine::new(test_config());
        engine.record_cancel_failure();
        engine.record_cancel_failure();
        let halted = engine.record_cancel_failure();
        assert!(halted);
        assert!(engine.is_halted());
    }

    #[test]
    fn cancel_failure_reset() {
        let engine = RiskEngine::new(test_config());
        engine.record_cancel_failure();
        engine.record_cancel_failure();
        engine.reset_cancel_failures();
        assert!(!engine.record_cancel_failure()); // counter back to 1
        assert!(!engine.is_halted());
    }

    #[test]
    fn resume_clears_halt() {
        let engine = RiskEngine::new(test_config());
        engine.halt("test");
        assert!(engine.is_halted());
        engine.resume();
        assert!(!engine.is_halted());
    }

    #[test]
    fn reset_daily_clears_halt_and_counters() {
        let engine = RiskEngine::new(test_config());
        engine.halt("test");
        engine.record_cancel_failure();
        engine.reset_daily();
        assert!(!engine.is_halted());
    }
}
