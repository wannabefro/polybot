//! Behavioral mean reversion strategy.
//!
//! Enters small positions when a market's price deviates significantly from
//! its recent moving average, expecting a reversion. Only operates in deep
//! markets. Hard constraints from the plan:
//!   - Only markets with 24h volume > config.mean_revert_min_volume_24h
//!   - Max position: config.mean_revert_max_nav_frac * NAV
//!   - Time-stop: exit after N hours if not profitable
//!   - Price-stop: exit at X% loss

use std::collections::HashMap;
use std::time::{Duration, Instant};

use polymarket_client_sdk::clob::types::{OrderType, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::market::book::BookStore;
use crate::market::discovery::TradableMarket;
use crate::order::pipeline::OrderIntent;
use crate::risk::guardrails::{RiskEngine, RiskVerdict};

/// How many price samples to keep for the simple moving average.
const SMA_WINDOW: usize = 60; // ~60 minutes at 1-sample-per-minute

/// Time-stop: close after this many hours without profit.
const TIME_STOP_HOURS: u64 = 4;

/// Price-stop: close if the position is down more than this fraction.
const PRICE_STOP_FRACTION: Decimal = dec!(0.03); // 3%

/// Minimum deviation from SMA to trigger entry (in decimal, e.g. 0.05 = 5%).
const MIN_DEVIATION: Decimal = dec!(0.05);

/// Moving average tracker for a single token.
#[derive(Debug, Clone)]
pub struct PriceTracker {
    samples: Vec<Decimal>,
    max_samples: usize,
}

impl PriceTracker {
    pub fn new(max_samples: usize) -> Self {
        Self {
            samples: Vec::with_capacity(max_samples),
            max_samples,
        }
    }

    pub fn push(&mut self, price: Decimal) {
        if self.samples.len() >= self.max_samples {
            self.samples.remove(0);
        }
        self.samples.push(price);
    }

    pub fn sma(&self) -> Option<Decimal> {
        if self.samples.is_empty() {
            return None;
        }
        let sum: Decimal = self.samples.iter().copied().sum();
        let count = Decimal::from(self.samples.len() as u64);
        Some(sum / count)
    }

    pub fn is_ready(&self) -> bool {
        // Require at least 10 samples for any signal
        self.samples.len() >= 10
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }
}

/// An open mean-reversion position to track for stop conditions.
#[derive(Debug, Clone)]
pub struct MeanRevertPosition {
    pub token_id: String,
    pub side: Side,
    pub entry_price: Decimal,
    pub size: Decimal,
    pub opened_at: Instant,
}

impl MeanRevertPosition {
    /// Check if the time-stop has triggered.
    pub fn time_stopped(&self, max_hours: u64) -> bool {
        self.opened_at.elapsed() > Duration::from_secs(max_hours * 3600)
    }

    /// Check if the price-stop has triggered.
    pub fn price_stopped(&self, current_price: Decimal, max_loss_frac: Decimal) -> bool {
        let pnl_frac = match self.side {
            Side::Buy => (current_price - self.entry_price) / self.entry_price,
            Side::Sell => (self.entry_price - current_price) / self.entry_price,
            _ => return false,
        };
        pnl_frac < -max_loss_frac
    }
}

/// State for the mean-reversion strategy across all tracked markets.
pub struct MeanRevertState {
    trackers: HashMap<String, PriceTracker>,
    positions: Vec<MeanRevertPosition>,
}

impl MeanRevertState {
    pub fn new() -> Self {
        Self {
            trackers: HashMap::new(),
            positions: Vec::new(),
        }
    }

    /// Record a price sample for a token.
    pub fn record_price(&mut self, token_id: &str, price: Decimal) {
        self.trackers
            .entry(token_id.to_string())
            .or_insert_with(|| PriceTracker::new(SMA_WINDOW))
            .push(price);
    }

    /// Get current SMA for a token.
    pub fn sma(&self, token_id: &str) -> Option<Decimal> {
        self.trackers.get(token_id)?.sma()
    }

    /// Check if tracker is ready for signals.
    pub fn is_ready(&self, token_id: &str) -> bool {
        self.trackers
            .get(token_id)
            .map_or(false, |t| t.is_ready())
    }

    /// Add an open position.
    pub fn open_position(&mut self, pos: MeanRevertPosition) {
        self.positions.push(pos);
    }

    /// Get all positions needing exit (time-stopped or price-stopped).
    pub fn positions_to_close(&self, books: &BookStore) -> Vec<OrderIntent> {
        let mut exits = Vec::new();
        for pos in &self.positions {
            let should_close = pos.time_stopped(TIME_STOP_HOURS)
                || books.get(&pos.token_id).and_then(|b| b.mid_price()).map_or(
                    false,
                    |mid| pos.price_stopped(mid, PRICE_STOP_FRACTION),
                );

            if should_close {
                let exit_side = match pos.side {
                    Side::Buy => Side::Sell,
                    Side::Sell => Side::Buy,
                    _ => continue,
                };
                exits.push(OrderIntent {
                    token_id: pos.token_id.clone(),
                    side: exit_side,
                    price: Decimal::ZERO, // will be market order (FAK)
                    size: pos.size,
                    order_type: OrderType::FOK,
                    post_only: false,
                    neg_risk: false,
                    fee_rate_bps: Decimal::ZERO,
                });
            }
        }
        exits
    }

    /// Remove closed positions by token_id.
    pub fn remove_position(&mut self, token_id: &str) {
        self.positions.retain(|p| p.token_id != token_id);
    }

    pub fn open_positions(&self) -> &[MeanRevertPosition] {
        &self.positions
    }

    /// Clean up trackers for tokens no longer in the universe.
    pub fn retain_tokens(&mut self, active_tokens: &[String]) {
        self.trackers.retain(|k, _| active_tokens.contains(k));
    }
}

/// Evaluate whether to enter a mean-reversion trade for a given market.
///
/// Returns Some(OrderIntent) if a trade should be placed, None otherwise.
pub fn evaluate_entry(
    config: &Config,
    market: &TradableMarket,
    books: &BookStore,
    risk: &RiskEngine,
    state: &MeanRevertState,
) -> Option<OrderIntent> {
    // Filter: deep markets only
    if market.volume_24h < config.mean_revert_min_volume_24h {
        debug!(
            market = %market.question,
            volume = market.volume_24h,
            min = config.mean_revert_min_volume_24h,
            "mean-revert: skipping low-volume market"
        );
        return None;
    }

    if market.tokens.is_empty() {
        return None;
    }

    let token_id = &market.tokens[0].token_id;

    // Need enough price history
    if !state.is_ready(token_id) {
        return None;
    }

    let book = books.get(token_id)?;
    let mid = book.mid_price()?;
    let sma = state.sma(token_id)?;

    if sma.is_zero() {
        return None;
    }

    let deviation = (mid - sma) / sma;

    // Not enough deviation
    if deviation.abs() < MIN_DEVIATION {
        return None;
    }

    // Determine direction: if price is above SMA, expect it to drop (sell)
    let side = if deviation > Decimal::ZERO {
        Side::Sell
    } else {
        Side::Buy
    };

    // Size: capped at mean_revert_max_nav_frac * NAV
    let max_notional = Decimal::try_from(config.nav_limit(config.mean_revert_max_nav_frac)).ok()?;
    let size = (max_notional / mid).min(dec!(100)); // cap shares

    if size < market.min_order_size {
        return None;
    }

    let intent = OrderIntent {
        token_id: token_id.clone(),
        side,
        price: mid, // enter at mid
        size,
        order_type: OrderType::GTC,
        post_only: false,
        neg_risk: market.neg_risk,
        fee_rate_bps: market.maker_fee_bps,
    };

    // Risk check
    if let RiskVerdict::Rejected(reason) = risk.check(&market.condition_id, &intent) {
        debug!(
            market = %market.question,
            reason,
            "mean-revert: rejected by risk"
        );
        return None;
    }

    info!(
        market = %market.question,
        side = ?side,
        deviation = %deviation,
        size = %size,
        "mean-revert: entry signal"
    );

    Some(intent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::test_config;
    use crate::market::book::BookStore;
    use crate::market::discovery::{TradableMarket, TokenInfo};
    use crate::risk::guardrails::RiskEngine;
    use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, OrderBookLevel};
    use polymarket_client_sdk::types::{B256, U256};

    fn make_market(volume: f64) -> TradableMarket {
        TradableMarket {
            condition_id: "cond1".into(),
            question: "Test market?".into(),
            tokens: vec![TokenInfo {
                token_id: "token1".into(),
                outcome: "Yes".into(),
                price: dec!(0.50),
            }],
            neg_risk: false,
            neg_risk_market_id: None,
            min_tick_size: dec!(0.01),
            min_order_size: dec!(1),
            maker_fee_bps: dec!(0),
            rewards_active: false,
            rewards_max_spread: None,
            rewards_min_size: None,
            volume_24h: volume,
            tags: vec![],
        }
    }

    fn make_book_store(bid: &str, ask: &str) -> BookStore {
        let store = BookStore::new();
        let update = BookUpdate::builder()
            .asset_id(U256::ZERO)
            .market(B256::ZERO)
            .timestamp(0)
            .bids(vec![OrderBookLevel::builder()
                .price(bid.parse::<Decimal>().unwrap())
                .size(dec!(100))
                .build()])
            .asks(vec![OrderBookLevel::builder()
                .price(ask.parse::<Decimal>().unwrap())
                .size(dec!(100))
                .build()])
            .build();
        store.apply("token1", &update);
        store
    }

    #[test]
    fn price_tracker_sma() {
        let mut tracker = PriceTracker::new(5);
        tracker.push(dec!(10));
        tracker.push(dec!(20));
        tracker.push(dec!(30));
        assert_eq!(tracker.sma(), Some(dec!(20)));
        assert_eq!(tracker.len(), 3);
    }

    #[test]
    fn price_tracker_window_eviction() {
        let mut tracker = PriceTracker::new(3);
        tracker.push(dec!(10));
        tracker.push(dec!(20));
        tracker.push(dec!(30));
        tracker.push(dec!(40));
        // Should have evicted 10, now [20, 30, 40]
        assert_eq!(tracker.sma(), Some(dec!(30)));
        assert_eq!(tracker.len(), 3);
    }

    #[test]
    fn price_tracker_empty_sma() {
        let tracker = PriceTracker::new(5);
        assert_eq!(tracker.sma(), None);
        assert!(!tracker.is_ready());
    }

    #[test]
    fn price_tracker_readiness() {
        let mut tracker = PriceTracker::new(60);
        for i in 0..9 {
            tracker.push(Decimal::from(i));
        }
        assert!(!tracker.is_ready());
        tracker.push(dec!(9));
        assert!(tracker.is_ready());
    }

    #[test]
    fn mean_revert_state_record_and_sma() {
        let mut state = MeanRevertState::new();
        state.record_price("t1", dec!(100));
        state.record_price("t1", dec!(200));
        assert_eq!(state.sma("t1"), Some(dec!(150)));
        assert_eq!(state.sma("t2"), None);
    }

    #[test]
    fn mean_revert_state_retain_tokens() {
        let mut state = MeanRevertState::new();
        state.record_price("t1", dec!(100));
        state.record_price("t2", dec!(200));
        state.retain_tokens(&["t1".into()]);
        assert!(state.sma("t1").is_some());
        assert!(state.sma("t2").is_none());
    }

    #[test]
    fn position_time_stop() {
        let pos = MeanRevertPosition {
            token_id: "t1".into(),
            side: Side::Buy,
            entry_price: dec!(0.50),
            size: dec!(10),
            opened_at: Instant::now() - Duration::from_secs(TIME_STOP_HOURS * 3600 + 1),
        };
        assert!(pos.time_stopped(TIME_STOP_HOURS));

        let fresh = MeanRevertPosition {
            opened_at: Instant::now(),
            ..pos.clone()
        };
        assert!(!fresh.time_stopped(TIME_STOP_HOURS));
    }

    #[test]
    fn position_price_stop_buy() {
        let pos = MeanRevertPosition {
            token_id: "t1".into(),
            side: Side::Buy,
            entry_price: dec!(1.00),
            size: dec!(10),
            opened_at: Instant::now(),
        };
        // 4% loss → should trigger at 3% threshold
        assert!(pos.price_stopped(dec!(0.96), dec!(0.03)));
        // 2% loss → should not trigger
        assert!(!pos.price_stopped(dec!(0.98), dec!(0.03)));
        // Price went up → no stop
        assert!(!pos.price_stopped(dec!(1.05), dec!(0.03)));
    }

    #[test]
    fn position_price_stop_sell() {
        let pos = MeanRevertPosition {
            token_id: "t1".into(),
            side: Side::Sell,
            entry_price: dec!(1.00),
            size: dec!(10),
            opened_at: Instant::now(),
        };
        // Price went up 4% → loss for short
        assert!(pos.price_stopped(dec!(1.04), dec!(0.03)));
        // Price went down → profit for short
        assert!(!pos.price_stopped(dec!(0.95), dec!(0.03)));
    }

    #[test]
    fn positions_to_close_time_stop() {
        let mut state = MeanRevertState::new();
        let books = make_book_store("0.48", "0.52");
        state.open_position(MeanRevertPosition {
            token_id: "token1".into(),
            side: Side::Buy,
            entry_price: dec!(0.50),
            size: dec!(10),
            opened_at: Instant::now() - Duration::from_secs(TIME_STOP_HOURS * 3600 + 1),
        });

        let exits = state.positions_to_close(&books);
        assert_eq!(exits.len(), 1);
        assert!(matches!(exits[0].side, Side::Sell)); // exit a buy
    }

    #[test]
    fn positions_to_close_none_when_fresh() {
        let mut state = MeanRevertState::new();
        let books = make_book_store("0.48", "0.52");
        state.open_position(MeanRevertPosition {
            token_id: "token1".into(),
            side: Side::Buy,
            entry_price: dec!(0.50),
            size: dec!(10),
            opened_at: Instant::now(),
        });

        let exits = state.positions_to_close(&books);
        assert!(exits.is_empty());
    }

    #[test]
    fn remove_position() {
        let mut state = MeanRevertState::new();
        state.open_position(MeanRevertPosition {
            token_id: "t1".into(),
            side: Side::Buy,
            entry_price: dec!(0.50),
            size: dec!(10),
            opened_at: Instant::now(),
        });
        assert_eq!(state.open_positions().len(), 1);
        state.remove_position("t1");
        assert!(state.open_positions().is_empty());
    }

    #[test]
    fn evaluate_entry_rejects_low_volume() {
        let config = test_config();
        let market = make_market(100.0); // below 10_000 threshold
        let books = make_book_store("0.48", "0.52");
        let risk = RiskEngine::new(config.clone());
        let state = MeanRevertState::new();

        assert!(evaluate_entry(&config, &market, &books, &risk, &state).is_none());
    }

    #[test]
    fn evaluate_entry_rejects_not_ready() {
        let config = test_config();
        let market = make_market(50_000.0);
        let books = make_book_store("0.48", "0.52");
        let risk = RiskEngine::new(config.clone());
        let mut state = MeanRevertState::new();

        // Only 5 samples, need 10
        for i in 0..5 {
            state.record_price("token1", Decimal::from(50 + i));
        }

        assert!(evaluate_entry(&config, &market, &books, &risk, &state).is_none());
    }

    #[test]
    fn evaluate_entry_rejects_small_deviation() {
        let config = test_config();
        let market = make_market(50_000.0);
        let books = make_book_store("0.49", "0.51");
        let risk = RiskEngine::new(config.clone());
        let mut state = MeanRevertState::new();

        // SMA ≈ 0.50, mid = 0.50, deviation ≈ 0 → no entry
        for _ in 0..15 {
            state.record_price("token1", dec!(0.50));
        }

        assert!(evaluate_entry(&config, &market, &books, &risk, &state).is_none());
    }

    #[test]
    fn evaluate_entry_generates_sell_when_above_sma() {
        let config = test_config();
        let market = make_market(50_000.0);
        // Mid = 0.60, way above SMA
        let books = make_book_store("0.59", "0.61");
        let risk = RiskEngine::new(config.clone());
        let mut state = MeanRevertState::new();

        // SMA of ~0.50
        for _ in 0..15 {
            state.record_price("token1", dec!(0.50));
        }

        let intent = evaluate_entry(&config, &market, &books, &risk, &state);
        assert!(intent.is_some());
        let intent = intent.unwrap();
        assert!(matches!(intent.side, Side::Sell));
        assert_eq!(intent.token_id, "token1");
    }

    #[test]
    fn evaluate_entry_generates_buy_when_below_sma() {
        let config = test_config();
        let market = make_market(50_000.0);
        // Mid = 0.40, below SMA
        let books = make_book_store("0.39", "0.41");
        let risk = RiskEngine::new(config.clone());
        let mut state = MeanRevertState::new();

        // SMA of ~0.50
        for _ in 0..15 {
            state.record_price("token1", dec!(0.50));
        }

        let intent = evaluate_entry(&config, &market, &books, &risk, &state);
        assert!(intent.is_some());
        assert!(matches!(intent.unwrap().side, Side::Buy));
    }

    #[test]
    fn evaluate_entry_empty_tokens() {
        let config = test_config();
        let mut market = make_market(50_000.0);
        market.tokens.clear();
        let books = make_book_store("0.49", "0.51");
        let risk = RiskEngine::new(config.clone());
        let state = MeanRevertState::new();

        assert!(evaluate_entry(&config, &market, &books, &risk, &state).is_none());
    }
}
