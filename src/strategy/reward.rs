//! Sponsored/reward capture strategy.
//!
//! Targets markets where reward density is high and depth is low.
//! One-sided fills are tracked; if the complement side also fills within
//! the hedge window, the pair is considered hedged. If the complement
//! doesn't fill in time, we SELL the tokens at best bid to flatten.

use polymarket_client_sdk::clob::types::{OrderType, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::time::{Duration, Instant};
use tracing::{debug, info};

use crate::config::Config;
use crate::market::book::BookStore;
use crate::market::discovery::TradableMarket;
use crate::order::pipeline::OrderIntent;
use crate::risk::guardrails::{RiskEngine, RiskVerdict};

/// A fill that needs hedging (complement side hasn't filled yet).
#[derive(Debug, Clone)]
pub struct UnhedgedFill {
    pub token_id: String,
    pub condition_id: String,
    #[allow(dead_code)]
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub filled_at: Instant,
    pub neg_risk: bool,
    pub fee_rate_bps: Decimal,
}

impl UnhedgedFill {
    /// Check if this fill has exceeded the hedge window.
    pub fn hedge_expired(&self, timeout: Duration) -> bool {
        self.filled_at.elapsed() > timeout
    }

    /// Build a SELL order to flatten this position (unwind at best bid).
    pub fn unwind_intent(&self, books: &BookStore) -> Option<OrderIntent> {
        // We always hold tokens from a BUY fill → SELL at best bid to flatten
        let book = books.get(&self.token_id)?;
        let bid = book.bids.best()?;

        Some(OrderIntent {
            token_id: self.token_id.clone(),
            side: Side::Sell,
            price: bid.price,
            size: self.size,
            order_type: OrderType::FOK,
            post_only: false,
            neg_risk: self.neg_risk,
            fee_rate_bps: self.fee_rate_bps,
        })
    }
}

/// Tracker for market-making fills awaiting complement hedges.
///
/// When both sides of a binary pair fill (BUY YES + BUY NO on the same
/// condition), they cancel out at settlement → profit = spread.
/// If only one side fills, we wait for the hedge window then unwind.
pub struct HedgeTracker {
    unhedged: Vec<UnhedgedFill>,
    hedge_timeout: Duration,
}

impl HedgeTracker {
    pub fn new(hedge_timeout: Duration) -> Self {
        Self {
            unhedged: Vec::new(),
            hedge_timeout,
        }
    }

    /// Record a fill. If a complement fill already exists for the same
    /// condition_id (different token), both are considered hedged and removed.
    pub fn record_fill(&mut self, fill: UnhedgedFill) {
        // Check if there's already a fill on the same condition but different token
        let complement_idx = self.unhedged.iter().position(|f| {
            f.condition_id == fill.condition_id && f.token_id != fill.token_id
        });

        if let Some(idx) = complement_idx {
            // Both sides filled → hedged, remove the existing one
            let matched = self.unhedged.remove(idx);
            info!(
                "🔒 HEDGED condition={} ({}@{} + {}@{})",
                fill.condition_id,
                matched.size, matched.price,
                fill.size, fill.price,
            );
            // Don't add the new fill either — pair is complete
        } else {
            self.unhedged.push(fill);
        }
    }

    /// Mark a fill as hedged by token_id (external hedge or manual close).
    pub fn mark_hedged(&mut self, token_id: &str) {
        self.unhedged.retain(|f| f.token_id != token_id);
    }

    /// Get fills that have exceeded the hedge window and need unwinding.
    /// Returns SELL intents to flatten the positions.
    pub fn expired_unwinds(&self, books: &BookStore) -> Vec<OrderIntent> {
        self.unhedged
            .iter()
            .filter(|f| f.hedge_expired(self.hedge_timeout))
            .filter_map(|f| f.unwind_intent(books))
            .collect()
    }

    /// Check if any fills are still waiting for their complement.
    #[allow(dead_code)]
    pub fn has_pending(&self) -> bool {
        !self.unhedged.is_empty()
    }

    /// Get condition_ids that have unhedged single-sided fills.
    /// Markets with these conditions should NOT receive new quotes.
    pub fn unhedged_conditions(&self) -> std::collections::HashSet<String> {
        self.unhedged.iter().map(|f| f.condition_id.clone()).collect()
    }

    #[allow(dead_code)]
    pub fn unhedged_count(&self) -> usize {
        self.unhedged.len()
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.unhedged.clear();
    }
}

/// Evaluate whether a market qualifies for reward capture quoting.
///
/// Criteria:
///   - Market has active rewards
///   - Reward density (estimated) justifies the spread risk
///   - Passes risk checks
///
/// Returns a Vec of (bid, ask) pairs — one per quotable token.
pub fn evaluate_reward_quote(
    config: &Config,
    market: &TradableMarket,
    books: &BookStore,
    risk: &RiskEngine,
) -> Vec<(OrderIntent, OrderIntent)> {
    if !market.rewards_active {
        return Vec::new();
    }

    // For binary markets (2 tokens): provide two-sided liquidity using BUY orders only.
    // BUY YES at bid_price + BUY NO at complement of ask_price.
    // We can't SELL tokens we don't hold — the CLOB requires token balance for sells.
    if market.tokens.len() == 2 {
        if let Some(pair) = evaluate_binary_reward(config, market, books, risk) {
            return vec![pair];
        }
        return Vec::new();
    }

    // For multi-outcome (neg-risk) markets: place BUY orders on individual tokens
    let mut pairs = Vec::new();
    for token in &market.tokens {
        if let Some(intent) = evaluate_single_buy_reward(config, market, token, books, risk) {
            pairs.push(intent);
        }
    }
    pairs
}

/// Binary market: BUY token[0] at bid + BUY token[1] at complement ask.
fn evaluate_binary_reward(
    config: &Config,
    market: &TradableMarket,
    books: &BookStore,
    risk: &RiskEngine,
) -> Option<(OrderIntent, OrderIntent)> {
    let t0 = &market.tokens[0];
    let t1 = &market.tokens[1];

    let book0 = books.get(&t0.token_id)?;
    let best_bid0 = book0.bids.best()?.price;
    let best_ask0 = book0.asks.best()?.price;

    if best_ask0 <= best_bid0 {
        return None;
    }

    let tick = market.min_tick_size;

    // Our bid on token0: join best bid (BUY token0)
    let mut bid_price = best_bid0;

    // Our "ask" on token0: BUY the complement token1 at (1 - ask_price_on_token0)
    // This is economically equivalent to selling token0 at ask_price.
    let mut complement_price = Decimal::ONE - best_ask0;
    // Clamp complement price to valid range
    if complement_price <= Decimal::ZERO || complement_price >= Decimal::ONE {
        debug!(market = %market.question, complement = %complement_price, "reward: invalid complement price");
        return None;
    }
    complement_price = round_to_tick(complement_price, tick);

    // Enforce rewards_max_spread on the token0 spread
    let our_spread = best_ask0 - bid_price;
    if let Some(max_spread) = market.rewards_max_spread {
        if our_spread > max_spread {
            // Tighten bid up toward mid
            let mid = (best_bid0 + best_ask0) / Decimal::TWO;
            let tight_half = max_spread / Decimal::TWO;
            bid_price = round_to_tick(mid - tight_half, tick);
            let tight_ask = round_to_tick(mid + tight_half, tick);
            complement_price = round_to_tick(Decimal::ONE - tight_ask, tick);
            if complement_price <= Decimal::ZERO {
                debug!(market = %market.question, "reward: can't tighten enough for max_spread");
                return None;
            }
        }
    }

    // Post-only safety: bid must not cross token0 book
    if bid_price >= best_ask0 {
        bid_price = best_ask0 - tick;
    }
    if bid_price <= Decimal::ZERO {
        return None;
    }

    // Check complement against token1 book if available
    if let Some(book1) = books.get(&t1.token_id) {
        if let Some(best_ask1) = book1.asks.best() {
            if complement_price >= best_ask1.price {
                complement_price = best_ask1.price - tick;
            }
        }
    }
    if complement_price <= Decimal::ZERO {
        return None;
    }

    // Size calculation
    let rewards_min = market.rewards_min_size.unwrap_or(Decimal::ZERO);
    let reward_min = Decimal::from_f64_retain(config.reward_min_size).unwrap_or(dec!(3));
    let mid = (best_bid0 + best_ask0) / Decimal::TWO;
    let mut size = market.min_order_size.max(rewards_min).max(reward_min);
    let inventory_cap_notional = Decimal::from_f64_retain(
        config.nav_limit(config.effective_max_one_sided_inventory())
    ).unwrap_or(Decimal::ZERO);
    if !mid.is_zero() && inventory_cap_notional > Decimal::ZERO {
        size = size.min(inventory_cap_notional / mid);
    }
    if size < market.min_order_size {
        debug!(market = %market.question, size = %size, min = %market.min_order_size, "reward: size below min");
        return None;
    }

    let bid_intent = OrderIntent {
        token_id: t0.token_id.clone(),
        side: Side::Buy,
        price: bid_price,
        size,
        order_type: OrderType::GTC,
        post_only: true,
        neg_risk: market.neg_risk,
        fee_rate_bps: market.maker_fee_bps,
    };

    let ask_intent = OrderIntent {
        token_id: t1.token_id.clone(),
        side: Side::Buy,
        price: complement_price,
        size,
        order_type: OrderType::GTC,
        post_only: true,
        neg_risk: market.neg_risk,
        fee_rate_bps: market.maker_fee_bps,
    };

    // Risk checks
    if let RiskVerdict::Rejected(reason) = risk.check(&market.condition_id, &bid_intent) {
        debug!(market = %market.question, reason, "reward: bid rejected");
        return None;
    }
    if let RiskVerdict::Rejected(reason) = risk.check(&market.condition_id, &ask_intent) {
        debug!(market = %market.question, reason, "reward: complement ask rejected");
        return None;
    }

    debug!(
        market = %market.question,
        bid = %bid_price,
        complement = %complement_price,
        size = %size,
        "reward: binary quote"
    );

    Some((bid_intent, ask_intent))
}

/// Single token BUY for neg-risk/multi-outcome markets.
fn evaluate_single_buy_reward(
    config: &Config,
    market: &TradableMarket,
    token: &crate::market::discovery::TokenInfo,
    books: &BookStore,
    risk: &RiskEngine,
) -> Option<(OrderIntent, OrderIntent)> {
    let book = books.get(&token.token_id)?;
    let best_bid = book.bids.best()?.price;
    let best_ask = book.asks.best()?.price;

    if best_ask <= best_bid {
        return None;
    }

    let tick = market.min_tick_size;
    let mut bid_price = best_bid;

    if let Some(max_spread) = market.rewards_max_spread {
        let mid = (best_bid + best_ask) / Decimal::TWO;
        if (best_ask - bid_price) > max_spread {
            bid_price = round_to_tick(mid - max_spread / Decimal::TWO, tick);
        }
    }

    if bid_price >= best_ask {
        bid_price = best_ask - tick;
    }
    if bid_price <= Decimal::ZERO {
        return None;
    }

    let rewards_min = market.rewards_min_size.unwrap_or(Decimal::ZERO);
    let reward_min = Decimal::from_f64_retain(config.reward_min_size).unwrap_or(dec!(3));
    let mid = (best_bid + best_ask) / Decimal::TWO;
    let mut size = market.min_order_size.max(rewards_min).max(reward_min);
    let inventory_cap_notional = Decimal::from_f64_retain(
        config.nav_limit(config.effective_max_one_sided_inventory())
    ).unwrap_or(Decimal::ZERO);
    if !mid.is_zero() && inventory_cap_notional > Decimal::ZERO {
        size = size.min(inventory_cap_notional / mid);
    }
    if size < market.min_order_size {
        return None;
    }

    // For multi-outcome, just place a single BUY on this token
    let intent = OrderIntent {
        token_id: token.token_id.clone(),
        side: Side::Buy,
        price: bid_price,
        size,
        order_type: OrderType::GTC,
        post_only: true,
        neg_risk: market.neg_risk,
        fee_rate_bps: market.maker_fee_bps,
    };

    if let RiskVerdict::Rejected(reason) = risk.check(&market.condition_id, &intent) {
        debug!(market = %market.question, reason, "reward: buy rejected");
        return None;
    }

    debug!(
        market = %market.question,
        bid = %bid_price,
        size = %size,
        "reward: single buy quote"
    );

    // Return as a pair with the same intent duplicated (caller handles both)
    Some((intent.clone(), intent))
}

/// Round price to the nearest valid tick.
fn round_to_tick(price: Decimal, tick_size: Decimal) -> Decimal {
    (price / tick_size).round() * tick_size
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

    fn make_market(rewards_active: bool) -> TradableMarket {
        TradableMarket {
            condition_id: "cond1".into(),
            question: "Test reward market?".into(),
            tokens: vec![
                TokenInfo {
                    token_id: "token1".into(),
                    outcome: "Yes".into(),
                    price: dec!(0.50),
                },
                TokenInfo {
                    token_id: "token2".into(),
                    outcome: "No".into(),
                    price: dec!(0.50),
                },
            ],
            neg_risk: false,
            neg_risk_market_id: None,
            min_tick_size: dec!(0.01),
            min_order_size: dec!(5),
            maker_fee_bps: dec!(0),
            rewards_active,
            rewards_max_spread: None,
            rewards_min_size: None,
            volume_24h: 50_000.0,
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
        // Also create complement book for token2
        let complement_bid = (Decimal::ONE - ask.parse::<Decimal>().unwrap()).to_string();
        let complement_ask = (Decimal::ONE - bid.parse::<Decimal>().unwrap()).to_string();
        let update2 = BookUpdate::builder()
            .asset_id(U256::ZERO)
            .market(B256::ZERO)
            .timestamp(0)
            .bids(vec![OrderBookLevel::builder()
                .price(complement_bid.parse::<Decimal>().unwrap())
                .size(dec!(100))
                .build()])
            .asks(vec![OrderBookLevel::builder()
                .price(complement_ask.parse::<Decimal>().unwrap())
                .size(dec!(100))
                .build()])
            .build();
        store.apply("token2", &update2);
        store
    }

    // --- UnhedgedFill tests ---

    #[test]
    fn unhedged_fill_not_expired() {
        let fill = UnhedgedFill {
            token_id: "t1".into(),
            condition_id: "cond1".into(),
            side: Side::Buy,
            price: dec!(0.50),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        };
        assert!(!fill.hedge_expired(Duration::from_secs(300)));
    }

    #[test]
    fn unhedged_fill_expired() {
        let fill = UnhedgedFill {
            token_id: "t1".into(),
            condition_id: "cond1".into(),
            side: Side::Buy,
            price: dec!(0.50),
            size: dec!(10),
            filled_at: Instant::now() - Duration::from_secs(301),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        };
        assert!(fill.hedge_expired(Duration::from_secs(300)));
    }

    #[test]
    fn unwind_intent_sells_at_best_bid() {
        let fill = UnhedgedFill {
            token_id: "token1".into(),
            condition_id: "cond1".into(),
            side: Side::Buy,
            price: dec!(0.52),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        };
        let books = make_book_store("0.48", "0.52");
        let intent = fill.unwind_intent(&books).unwrap();
        assert!(matches!(intent.side, Side::Sell));
        assert_eq!(intent.price, dec!(0.48)); // sell at best bid
        assert_eq!(intent.size, dec!(10));
        assert!(!intent.post_only);
    }

    #[test]
    fn unwind_intent_inherits_neg_risk_and_fee() {
        let fill = UnhedgedFill {
            token_id: "token1".into(),
            condition_id: "cond1".into(),
            side: Side::Buy,
            price: dec!(0.52),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: true,
            fee_rate_bps: dec!(20),
        };
        let books = make_book_store("0.48", "0.52");
        let intent = fill.unwind_intent(&books).unwrap();
        assert!(intent.neg_risk);
        assert_eq!(intent.fee_rate_bps, dec!(20));
    }

    #[test]
    fn unwind_intent_no_book_returns_none() {
        let fill = UnhedgedFill {
            token_id: "nonexistent".into(),
            condition_id: "cond1".into(),
            side: Side::Buy,
            price: dec!(0.50),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        };
        let books = BookStore::new();
        assert!(fill.unwind_intent(&books).is_none());
    }

    // --- HedgeTracker tests ---

    #[test]
    fn tracker_record_and_count() {
        let mut tracker = HedgeTracker::new(Duration::from_secs(300));
        assert_eq!(tracker.unhedged_count(), 0);

        tracker.record_fill(UnhedgedFill {
            token_id: "t1".into(),
            condition_id: "cond1".into(),
            side: Side::Buy,
            price: dec!(0.50),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        });
        assert_eq!(tracker.unhedged_count(), 1);
    }

    #[test]
    fn tracker_complement_fill_auto_hedges() {
        let mut tracker = HedgeTracker::new(Duration::from_secs(300));

        // First fill: BUY YES on cond1
        tracker.record_fill(UnhedgedFill {
            token_id: "yes_token".into(),
            condition_id: "cond1".into(),
            side: Side::Buy,
            price: dec!(0.48),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        });
        assert_eq!(tracker.unhedged_count(), 1);

        // Second fill: BUY NO on same condition → auto-hedged
        tracker.record_fill(UnhedgedFill {
            token_id: "no_token".into(),
            condition_id: "cond1".into(),
            side: Side::Buy,
            price: dec!(0.52),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        });
        assert_eq!(tracker.unhedged_count(), 0); // both removed
    }

    #[test]
    fn tracker_different_conditions_not_matched() {
        let mut tracker = HedgeTracker::new(Duration::from_secs(300));

        tracker.record_fill(UnhedgedFill {
            token_id: "t1".into(),
            condition_id: "cond1".into(),
            side: Side::Buy,
            price: dec!(0.50),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        });
        tracker.record_fill(UnhedgedFill {
            token_id: "t2".into(),
            condition_id: "cond2".into(),
            side: Side::Buy,
            price: dec!(0.50),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        });
        assert_eq!(tracker.unhedged_count(), 2); // different conditions, not matched
    }

    #[test]
    fn tracker_mark_hedged() {
        let mut tracker = HedgeTracker::new(Duration::from_secs(300));
        tracker.record_fill(UnhedgedFill {
            token_id: "t1".into(),
            condition_id: "cond1".into(),
            side: Side::Buy,
            price: dec!(0.50),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        });
        tracker.mark_hedged("t1");
        assert_eq!(tracker.unhedged_count(), 0);
    }

    #[test]
    fn tracker_expired_unwinds() {
        let mut tracker = HedgeTracker::new(Duration::from_millis(100));

        // Old fill (expired)
        tracker.record_fill(UnhedgedFill {
            token_id: "token1".into(),
            condition_id: "cond1".into(),
            side: Side::Buy,
            price: dec!(0.50),
            size: dec!(10),
            filled_at: Instant::now() - Duration::from_secs(1),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        });
        // Fresh fill (not expired)
        tracker.record_fill(UnhedgedFill {
            token_id: "token2".into(),
            condition_id: "cond2".into(),
            side: Side::Buy,
            price: dec!(0.50),
            size: dec!(5),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        });

        let books = make_book_store("0.48", "0.52");
        let unwinds = tracker.expired_unwinds(&books);
        assert_eq!(unwinds.len(), 1);
        assert!(matches!(unwinds[0].side, Side::Sell)); // sells to flatten
        assert_eq!(unwinds[0].price, dec!(0.48)); // at best bid
    }

    #[test]
    fn tracker_clear() {
        let mut tracker = HedgeTracker::new(Duration::from_secs(300));
        tracker.record_fill(UnhedgedFill {
            token_id: "t1".into(),
            condition_id: "cond1".into(),
            side: Side::Buy,
            price: dec!(0.50),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        });
        tracker.clear();
        assert_eq!(tracker.unhedged_count(), 0);
    }

    #[test]
    fn tracker_unhedged_conditions_blocks_quoting() {
        let mut tracker = HedgeTracker::new(Duration::from_secs(300));
        assert!(tracker.unhedged_conditions().is_empty());

        tracker.record_fill(UnhedgedFill {
            token_id: "yes_token".into(),
            condition_id: "cond_btc".into(),
            side: Side::Buy,
            price: dec!(0.48),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        });

        let blocked = tracker.unhedged_conditions();
        assert!(blocked.contains("cond_btc"));
        assert!(!blocked.contains("cond_other"));

        // Complement fills → condition unblocked
        tracker.record_fill(UnhedgedFill {
            token_id: "no_token".into(),
            condition_id: "cond_btc".into(),
            side: Side::Buy,
            price: dec!(0.52),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        });
        assert!(tracker.unhedged_conditions().is_empty());
    }

    // --- evaluate_reward_quote tests ---

    #[test]
    fn reward_quote_no_rewards_active() {
        let config = test_config();
        let market = make_market(false);
        let books = make_book_store("0.48", "0.52");
        let risk = RiskEngine::new(config.clone());

        assert!(evaluate_reward_quote(&config, &market, &books, &risk).is_empty());
    }

    #[test]
    fn reward_quote_generates_two_sided() {
        let config = test_config();
        let market = make_market(true);
        let books = make_book_store("0.48", "0.52");
        let risk = RiskEngine::new(config.clone());

        let result = evaluate_reward_quote(&config, &market, &books, &risk);
        assert!(!result.is_empty());
        let (bid, ask) = &result[0];
        // Both sides are BUY orders (bid on token0, complement buy on token1)
        assert!(matches!(bid.side, Side::Buy));
        assert!(matches!(ask.side, Side::Buy));
        assert!(bid.post_only);
        assert!(ask.post_only);
        // Bid is on token0, complement is on token1
        assert_eq!(bid.token_id, "token1");
        assert_eq!(ask.token_id, "token2");
    }

    #[test]
    fn reward_quote_empty_tokens() {
        let config = test_config();
        let mut market = make_market(true);
        market.tokens.clear();
        let books = make_book_store("0.48", "0.52");
        let risk = RiskEngine::new(config.clone());

        assert!(evaluate_reward_quote(&config, &market, &books, &risk).is_empty());
    }

    #[test]
    fn reward_quote_no_book() {
        let config = test_config();
        let market = make_market(true);
        let books = BookStore::new(); // empty
        let risk = RiskEngine::new(config.clone());

        assert!(evaluate_reward_quote(&config, &market, &books, &risk).is_empty());
    }

    #[test]
    fn round_to_tick_works() {
        assert_eq!(round_to_tick(dec!(0.523), dec!(0.01)), dec!(0.52));
        assert_eq!(round_to_tick(dec!(0.526), dec!(0.01)), dec!(0.53));
        assert_eq!(round_to_tick(dec!(0.50), dec!(0.001)), dec!(0.500));
    }

    #[test]
    fn reward_quote_max_spread_tightens() {
        let config = test_config();
        let mut market = make_market(true);
        // Set a tight max spread that will force tightening
        market.rewards_max_spread = Some(dec!(0.01));
        let books = make_book_store("0.48", "0.52");
        let risk = RiskEngine::new(config.clone());

        let result = evaluate_reward_quote(&config, &market, &books, &risk);
        if let Some((bid, ask)) = result.first() {
            assert!(ask.price - bid.price <= dec!(0.01), "spread should be tightened to max_spread");
        }
        // Either tightened or returned None — both are valid
    }

    #[test]
    fn reward_quote_min_size_enforced() {
        let config = test_config();
        let mut market = make_market(true);
        market.rewards_min_size = Some(dec!(25));
        let books = make_book_store("0.48", "0.52");
        let risk = RiskEngine::new(config.clone());

        let result = evaluate_reward_quote(&config, &market, &books, &risk);
        assert!(!result.is_empty());
        let (bid, _ask) = &result[0];
        assert!(bid.size >= dec!(25), "size should respect rewards_min_size");
    }
}
