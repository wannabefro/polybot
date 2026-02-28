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
    /// Pre-computed Decimal risk limits (avoids f64→Decimal on every check).
    /// Wrapped in RwLock so update_nav() can recalculate them live.
    limit_per_market: RwLock<Decimal>,
    limit_gross: RwLock<Decimal>,
    limit_daily_loss: RwLock<Decimal>,
    limit_inventory: RwLock<Decimal>,
    /// Per-market net notional exposure (condition_id → signed USDC notional).
    /// Positive = net long, negative = net short.
    market_exposure: RwLock<HashMap<String, Decimal>>,
    /// Per-token directional exposure (token_id → signed size: +buy, -sell).
    token_inventory: RwLock<HashMap<String, Decimal>>,
    /// Per-token cumulative cashflow (sell proceeds - buy spend).
    token_cashflow: RwLock<HashMap<String, Decimal>>,
    /// Daily realized P&L tracker.
    daily_pnl: RwLock<Decimal>,
    /// Emergency halt flag.
    halted: AtomicBool,
    /// Consecutive cancel failure counter.
    cancel_failures: AtomicU32,
}

/// Safely convert f64 NAV limit to Decimal. Panics on NaN/Inf at startup
/// rather than silently bypassing limits at runtime.
fn safe_nav_decimal(nav: f64, fraction: f64, label: &str) -> Decimal {
    let val = nav * fraction;
    assert!(val.is_finite(), "risk limit {label} produced non-finite value: nav={nav}, fraction={fraction}");
    Decimal::from_f64_retain(val)
        .unwrap_or_else(|| panic!("risk limit {label} failed Decimal conversion: {val}"))
}

const CANCEL_FAILURE_HALT_THRESHOLD: u32 = 3;

impl RiskEngine {
    pub fn new(config: Config) -> Arc<Self> {
        let limit_per_market = safe_nav_decimal(
            config.nav_usdc,
            config.effective_max_notional_per_market(),
            "per_market",
        );
        let limit_gross = safe_nav_decimal(config.nav_usdc, config.effective_max_gross_exposure(), "gross");
        let limit_daily_loss = safe_nav_decimal(config.nav_usdc, config.daily_loss_stop, "daily_loss");
        let limit_inventory = safe_nav_decimal(
            config.nav_usdc,
            config.effective_max_one_sided_inventory(),
            "inventory",
        );

        Arc::new(Self {
            limit_per_market: RwLock::new(limit_per_market),
            limit_gross: RwLock::new(limit_gross),
            limit_daily_loss: RwLock::new(limit_daily_loss),
            limit_inventory: RwLock::new(limit_inventory),
            market_exposure: RwLock::new(HashMap::new()),
            token_inventory: RwLock::new(HashMap::new()),
            token_cashflow: RwLock::new(HashMap::new()),
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
        let delta_notional = match intent.side {
            Side::Buy => notional,
            Side::Sell => -notional,
            _ => Decimal::ZERO,
        };

        // 1. Per-market net notional cap (2% NAV default)
        let current = self
            .market_exposure
            .read()
            .get(condition_id)
            .copied()
            .unwrap_or(Decimal::ZERO);
        let projected = current + delta_notional;
        let lim_per_market = *self.limit_per_market.read();
        if projected.abs() > lim_per_market {
            return RiskVerdict::Rejected(format!(
                "per-market limit: |{projected}| > {lim_per_market}"
            ));
        }

        // 2. Gross exposure cap (25% NAV default)
        let gross: Decimal = self
            .market_exposure
            .read()
            .values()
            .map(|v| v.abs())
            .sum();
        let gross_projected = gross - current.abs() + projected.abs();
        let lim_gross = *self.limit_gross.read();
        if gross_projected > lim_gross {
            return RiskVerdict::Rejected(format!(
                "gross exposure limit: {gross_projected} > {lim_gross}"
            ));
        }

        // 3. Daily loss stop (3% NAV default)
        let pnl = *self.daily_pnl.read();
        let lim_daily = *self.limit_daily_loss.read();
        if pnl < -lim_daily {
            return RiskVerdict::Rejected(format!(
                "daily loss stop: P&L {pnl} < -{lim_daily}"
            ));
        }

        // 4. One-sided inventory cap (1% NAV default)
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
        let inv_notional = new_inv.abs() * intent.price;
        let lim_inv = *self.limit_inventory.read();
        if inv_notional > lim_inv {
            return RiskVerdict::Rejected(format!(
                "one-sided inventory: {inv_notional} > {lim_inv}"
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
        let signed_notional = match side {
            Side::Buy => notional,
            Side::Sell => -notional,
            _ => Decimal::ZERO,
        };
        {
            let mut map = self.market_exposure.write();
            let entry = map.entry(condition_id.to_string()).or_insert(Decimal::ZERO);
            *entry += signed_notional;
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
        {
            let mut cf = self.token_cashflow.write();
            let entry = cf.entry(token_id.to_string()).or_insert(Decimal::ZERO);
            match side {
                Side::Buy => *entry -= notional,
                Side::Sell => *entry += notional,
                _ => {}
            }
        }
    }

    /// Record realized P&L.
    #[allow(dead_code)]
    pub fn record_pnl(&self, amount: Decimal) {
        *self.daily_pnl.write() += amount;
    }

    /// Set current daily P&L directly (e.g., mark-to-market snapshot).
    #[allow(dead_code)]
    pub fn set_daily_pnl(&self, amount: Decimal) {
        *self.daily_pnl.write() = amount;
    }

    /// Compute mark-to-market P&L from inventory/cashflow and latest mid prices.
    #[allow(dead_code)]
    pub fn mark_to_market_pnl(&self, mids: &HashMap<String, Decimal>) -> Decimal {
        let inv = self.token_inventory.read();
        let cf = self.token_cashflow.read();

        let mut pnl = Decimal::ZERO;
        for (token_id, qty) in inv.iter() {
            let mid = mids.get(token_id).copied().unwrap_or(Decimal::ZERO);
            let cashflow = cf.get(token_id).copied().unwrap_or(Decimal::ZERO);
            pnl += cashflow + (*qty * mid);
        }
        pnl
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

    /// Snapshot of all token inventory (for position reconciliation).
    pub fn inventory_snapshot(&self) -> HashMap<String, Decimal> {
        self.token_inventory.read().clone()
    }

    /// Recalculate risk limits for a new NAV value.
    ///
    /// Called when the live NAV tracker detects a balance change.
    /// Uses the config fractions to recompute absolute USDC limits.
    pub fn update_nav(&self, new_nav: f64, config: &Config) {
        let mut cfg = config.clone();
        cfg.nav_usdc = new_nav;

        *self.limit_per_market.write() = safe_nav_decimal(
            new_nav,
            cfg.effective_max_notional_per_market(),
            "per_market",
        );
        *self.limit_gross.write() = safe_nav_decimal(
            new_nav,
            cfg.effective_max_gross_exposure(),
            "gross",
        );
        *self.limit_daily_loss.write() = safe_nav_decimal(
            new_nav,
            cfg.daily_loss_stop,
            "daily_loss",
        );
        *self.limit_inventory.write() = safe_nav_decimal(
            new_nav,
            cfg.effective_max_one_sided_inventory(),
            "inventory",
        );
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
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
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
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
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
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
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

    #[test]
    fn per_market_derisking_sell_is_allowed() {
        let engine = RiskEngine::new(test_config());
        // Build long exposure near the per-market cap (2% of 10_000 = 200).
        engine.record_fill("cond1", "token1", Side::Buy, Decimal::from(190), Decimal::from(190));

        // This sell reduces net market exposure from +190 to +90.
        let intent = sell_intent("token1", 1.0, 100.0);
        let verdict = engine.check("cond1", &intent);
        assert!(
            matches!(verdict, RiskVerdict::Approved),
            "de-risking order should be approved"
        );
    }

    #[test]
    fn gross_derisking_order_is_allowed_near_limit() {
        let engine = RiskEngine::new(test_config());

        // Gross exposure at 2450 (limit is 2500): twelve markets at +200 and one at +50.
        for i in 0..12 {
            engine.record_fill(
                &format!("cond{i}"),
                &format!("token{i}"),
                Side::Buy,
                Decimal::from(200),
                Decimal::from(200),
            );
        }
        engine.record_fill("cond_x", "token_x", Side::Buy, Decimal::from(50), Decimal::from(50));

        // Sell 80 on cond_x reduces net cond_x from +50 to -30 and gross from 2450 to 2430.
        let intent = OrderIntent {
            token_id: "token_x".into(),
            side: Side::Sell,
            price: Decimal::ONE,
            size: Decimal::from(80),
            order_type: OrderType::GTC,
            post_only: true,
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        };
        let verdict = engine.check("cond_x", &intent);
        assert!(
            matches!(verdict, RiskVerdict::Approved),
            "gross de-risking order should be approved"
        );
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

    // ── Phase 4 bug-fix tests ──

    #[test]
    fn inventory_snapshot_returns_all_tokens() {
        let engine = RiskEngine::new(test_config());
        engine.record_fill("cond1", "token_a", Side::Buy, Decimal::from(50), Decimal::from(25));
        engine.record_fill("cond1", "token_b", Side::Sell, Decimal::from(30), Decimal::from(15));

        let snapshot = engine.inventory_snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(*snapshot.get("token_a").unwrap(), Decimal::from(50));
        assert_eq!(*snapshot.get("token_b").unwrap(), Decimal::from(-30));
    }

    #[test]
    fn inventory_snapshot_empty_initially() {
        let engine = RiskEngine::new(test_config());
        let snapshot = engine.inventory_snapshot();
        assert!(snapshot.is_empty());
    }

    #[test]
    fn safe_nav_decimal_panics_on_nan() {
        let result = std::panic::catch_unwind(|| {
            safe_nav_decimal(f64::NAN, 0.02, "test");
        });
        assert!(result.is_err(), "NaN should panic");
    }

    #[test]
    fn safe_nav_decimal_panics_on_inf() {
        let result = std::panic::catch_unwind(|| {
            safe_nav_decimal(f64::INFINITY, 0.02, "test");
        });
        assert!(result.is_err(), "Infinity should panic");
    }

    #[test]
    fn safe_nav_decimal_valid_value() {
        let val = safe_nav_decimal(10000.0, 0.02, "test");
        assert!(val > Decimal::ZERO);
        assert_eq!(val, Decimal::from_f64_retain(200.0).unwrap());
    }

    #[test]
    fn pre_computed_limits_match_config() {
        let config = test_config();
        let engine = RiskEngine::new(config.clone());
        assert_eq!(*engine.limit_per_market.read(), safe_nav_decimal(config.nav_usdc, config.effective_max_notional_per_market(), "pm"));
        assert_eq!(*engine.limit_gross.read(), safe_nav_decimal(config.nav_usdc, config.effective_max_gross_exposure(), "ge"));
        assert_eq!(*engine.limit_daily_loss.read(), safe_nav_decimal(config.nav_usdc, config.daily_loss_stop, "dl"));
        assert_eq!(*engine.limit_inventory.read(), safe_nav_decimal(config.nav_usdc, config.effective_max_one_sided_inventory(), "inv"));
    }

    #[test]
    fn mark_to_market_pnl_tracks_inventory_and_cashflow() {
        let engine = RiskEngine::new(test_config());
        // Buy 100 @ 0.50 => cashflow -50, inventory +100
        engine.record_fill("cond1", "token1", Side::Buy, Decimal::from(100), Decimal::from(50));

        let mut mids = HashMap::new();
        mids.insert("token1".into(), "0.60".parse::<Decimal>().unwrap());

        // PnL = -50 + (100 * 0.60) = +10
        assert_eq!(engine.mark_to_market_pnl(&mids), Decimal::from(10));
    }

    #[test]
    fn update_nav_recalculates_limits() {
        let config = test_config();
        let engine = RiskEngine::new(config.clone());
        let old_per_market = *engine.limit_per_market.read();

        // Double the NAV
        engine.update_nav(20_000.0, &config);
        let new_per_market = *engine.limit_per_market.read();

        // Limits should roughly double
        assert!(new_per_market > old_per_market);
        assert_eq!(
            new_per_market,
            safe_nav_decimal(20_000.0, config.effective_max_notional_per_market(), "pm")
        );
    }

    #[test]
    fn update_nav_to_small_account_uses_boosted_caps() {
        let config = test_config();
        let engine = RiskEngine::new(config.clone());

        // Shrink to small account
        engine.update_nav(100.0, &config);
        let per_market = *engine.limit_per_market.read();

        // Small account at $100: 8% × 100 = $8
        let mut small_cfg = config.clone();
        small_cfg.nav_usdc = 100.0;
        assert_eq!(per_market, safe_nav_decimal(100.0, small_cfg.effective_max_notional_per_market(), "pm"));
    }

    #[test]
    fn set_daily_pnl_enforces_loss_stop() {
        let engine = RiskEngine::new(test_config());
        engine.set_daily_pnl(Decimal::from(-500)); // below 3% stop on 10k NAV
        let verdict = engine.check("cond1", &test_intent(0.50, 1.0));
        assert!(matches!(verdict, RiskVerdict::Rejected(_)));
    }
}
