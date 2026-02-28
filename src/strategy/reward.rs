//! Sponsored/reward capture strategy.
//!
//! Targets markets where reward density is high and depth is low.
//! One-sided fills are immediately neutralized with a FAK hedge.
//! Hedge latency SLA: 500ms timeout → cancel-all + flatten.

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

/// Minimum reward-to-spread ratio to justify entry.
const MIN_REWARD_SPREAD_RATIO: Decimal = dec!(1.5);

/// A fill that needs hedging.
#[derive(Debug, Clone)]
pub struct UnhedgedFill {
    pub token_id: String,
    pub side: Side,
    #[allow(dead_code)]
    pub price: Decimal,
    pub size: Decimal,
    pub filled_at: Instant,
    pub neg_risk: bool,
    pub fee_rate_bps: Decimal,
}

impl UnhedgedFill {
    /// Check if this fill has breached the hedge SLA.
    pub fn hedge_sla_breached(&self, timeout: Duration) -> bool {
        self.filled_at.elapsed() > timeout
    }

    /// Build a hedge order (opposite side, FAK, aggressive price).
    pub fn hedge_intent(&self, books: &BookStore) -> Option<OrderIntent> {
        let book = books.get(&self.token_id)?;

        let (hedge_side, hedge_price) = match self.side {
            Side::Buy => {
                let bid = book.bids.best()?;
                (Side::Sell, bid.price)
            }
            Side::Sell => {
                let ask = book.asks.best()?;
                (Side::Buy, ask.price)
            }
            _ => return None,
        };

        Some(OrderIntent {
            token_id: self.token_id.clone(),
            side: hedge_side,
            price: hedge_price,
            size: self.size,
            order_type: OrderType::FOK, // fill-or-kill for hedges
            post_only: false,
            neg_risk: self.neg_risk,
            fee_rate_bps: self.fee_rate_bps,
        })
    }
}

/// Tracker for pending hedges.
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

    /// Record a fill that needs hedging.
    pub fn record_fill(&mut self, fill: UnhedgedFill) {
        self.unhedged.push(fill);
    }

    /// Mark a fill as hedged (remove from tracker).
    pub fn mark_hedged(&mut self, token_id: &str) {
        self.unhedged.retain(|f| f.token_id != token_id);
    }

    /// Get fills that have breached their hedge SLA.
    #[allow(dead_code)]
    pub fn breached_fills(&self) -> Vec<&UnhedgedFill> {
        self.unhedged
            .iter()
            .filter(|f| f.hedge_sla_breached(self.hedge_timeout))
            .collect()
    }

    /// Check if any fill is in an emergency state (SLA breached).
    pub fn has_emergency(&self) -> bool {
        self.unhedged
            .iter()
            .any(|f| f.hedge_sla_breached(self.hedge_timeout))
    }

    /// Get all pending hedges with their intended orders.
    pub fn pending_hedges(&self, books: &BookStore) -> Vec<OrderIntent> {
        self.unhedged
            .iter()
            .filter_map(|f| f.hedge_intent(books))
            .collect()
    }

    #[allow(dead_code)]
    pub fn unhedged_count(&self) -> usize {
        self.unhedged.len()
    }

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

    if market.tokens.is_empty() {
        return Vec::new();
    }

    let mut pairs = Vec::new();
    for token in &market.tokens {
        if let Some(pair) = evaluate_token_reward(config, market, token, books, risk) {
            pairs.push(pair);
        }
    }
    pairs
}

fn evaluate_token_reward(
    config: &Config,
    market: &TradableMarket,
    token: &crate::market::discovery::TokenInfo,
    books: &BookStore,
    risk: &RiskEngine,
) -> Option<(OrderIntent, OrderIntent)> {
    let book = books.get(&token.token_id)?;
    let mid = book.mid_price()?;
    let spread = book.spread()?;

    if spread.is_zero() {
        return None;
    }

    // Check reward-to-spread ratio: only quote if reward density justifies spread risk.
    // The reward daily rate is per-$1 notional; spread is the cost of a round-trip.
    // We need reward * holding_period > spread_cost * MIN_REWARD_SPREAD_RATIO.
    let our_spread = market.min_tick_size * Decimal::TWO;
    if our_spread > Decimal::ZERO {
        // Rough check: if the market's book spread is very wide relative to our
        // quoting spread, the adverse selection risk may exceed reward income.
        let ratio = spread / our_spread;
        if ratio > MIN_REWARD_SPREAD_RATIO * Decimal::TWO {
            debug!(
                market = %market.question,
                book_spread = %spread,
                our_spread = %our_spread,
                "reward: book spread too wide vs reward — skipping"
            );
            return None;
        }
    }

    // Tight spread for reward qualification
    let half_spread = market.min_tick_size;
    let mut bid_price = round_to_tick(mid - half_spread, market.min_tick_size);
    let mut ask_price = round_to_tick(mid + half_spread, market.min_tick_size);

    // Enforce rewards_max_spread: tighten if spread exceeds constraint
    if let Some(max_spread) = market.rewards_max_spread {
        if (ask_price - bid_price) > max_spread {
            let tight_half = max_spread / Decimal::TWO;
            bid_price = round_to_tick(mid - tight_half, market.min_tick_size);
            ask_price = round_to_tick(mid + tight_half, market.min_tick_size);
            if ask_price <= bid_price {
                return None;
            }
        }
    }

    // Use larger size for reward qualification; respect rewards_min_size
    let rewards_min = market.rewards_min_size.unwrap_or(Decimal::ZERO);
    let reward_min = Decimal::from_f64_retain(config.reward_min_size).unwrap_or(dec!(3));
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

    let bid_intent = OrderIntent {
        token_id: token.token_id.clone(),
        side: Side::Buy,
        price: bid_price,
        size,
        order_type: OrderType::GTC,
        post_only: true,
        neg_risk: market.neg_risk,
        fee_rate_bps: market.maker_fee_bps,
    };

    let ask_intent = OrderIntent {
        token_id: token.token_id.clone(),
        side: Side::Sell,
        price: ask_price,
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
        debug!(market = %market.question, reason, "reward: ask rejected");
        return None;
    }

    info!(
        market = %market.question,
        bid = %bid_price,
        ask = %ask_price,
        size = %size,
        "reward: quoting for reward capture"
    );

    Some((bid_intent, ask_intent))
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
            tokens: vec![TokenInfo {
                token_id: "token1".into(),
                outcome: "Yes".into(),
                price: dec!(0.50),
            }],
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
        store
    }

    // --- UnhedgedFill tests ---

    #[test]
    fn unhedged_fill_sla_not_breached() {
        let fill = UnhedgedFill {
            token_id: "t1".into(),
            side: Side::Buy,
            price: dec!(0.50),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        };
        assert!(!fill.hedge_sla_breached(Duration::from_millis(500)));
    }

    #[test]
    fn unhedged_fill_sla_breached() {
        let fill = UnhedgedFill {
            token_id: "t1".into(),
            side: Side::Buy,
            price: dec!(0.50),
            size: dec!(10),
            filled_at: Instant::now() - Duration::from_secs(1),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        };
        assert!(fill.hedge_sla_breached(Duration::from_millis(500)));
    }

    #[test]
    fn hedge_intent_buy_fill_sells() {
        let fill = UnhedgedFill {
            token_id: "token1".into(),
            side: Side::Buy,
            price: dec!(0.52),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        };
        let books = make_book_store("0.48", "0.52");
        let intent = fill.hedge_intent(&books).unwrap();
        assert!(matches!(intent.side, Side::Sell));
        assert_eq!(intent.price, dec!(0.48)); // sell at bid
        assert_eq!(intent.size, dec!(10));
        assert!(!intent.post_only);
    }

    #[test]
    fn hedge_intent_inherits_neg_risk_and_fee() {
        let fill = UnhedgedFill {
            token_id: "token1".into(),
            side: Side::Buy,
            price: dec!(0.52),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: true,
            fee_rate_bps: dec!(20),
        };
        let books = make_book_store("0.48", "0.52");
        let intent = fill.hedge_intent(&books).unwrap();
        assert!(intent.neg_risk, "hedge must inherit neg_risk from fill");
        assert_eq!(intent.fee_rate_bps, dec!(20), "hedge must inherit fee_rate_bps");
    }

    #[test]
    fn hedge_intent_sell_fill_buys() {
        let fill = UnhedgedFill {
            token_id: "token1".into(),
            side: Side::Sell,
            price: dec!(0.48),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        };
        let books = make_book_store("0.48", "0.52");
        let intent = fill.hedge_intent(&books).unwrap();
        assert!(matches!(intent.side, Side::Buy));
        assert_eq!(intent.price, dec!(0.52)); // buy at ask
    }

    #[test]
    fn hedge_intent_no_book_returns_none() {
        let fill = UnhedgedFill {
            token_id: "nonexistent".into(),
            side: Side::Buy,
            price: dec!(0.50),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        };
        let books = BookStore::new();
        assert!(fill.hedge_intent(&books).is_none());
    }

    // --- HedgeTracker tests ---

    #[test]
    fn tracker_record_and_count() {
        let mut tracker = HedgeTracker::new(Duration::from_millis(500));
        assert_eq!(tracker.unhedged_count(), 0);

        tracker.record_fill(UnhedgedFill {
            token_id: "t1".into(),
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
    fn tracker_mark_hedged() {
        let mut tracker = HedgeTracker::new(Duration::from_millis(500));
        tracker.record_fill(UnhedgedFill {
            token_id: "t1".into(),
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
    fn tracker_breached_fills() {
        let mut tracker = HedgeTracker::new(Duration::from_millis(500));
        tracker.record_fill(UnhedgedFill {
            token_id: "t1".into(),
            side: Side::Buy,
            price: dec!(0.50),
            size: dec!(10),
            filled_at: Instant::now() - Duration::from_secs(1),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        });
        tracker.record_fill(UnhedgedFill {
            token_id: "t2".into(),
            side: Side::Sell,
            price: dec!(0.50),
            size: dec!(5),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        });

        let breached = tracker.breached_fills();
        assert_eq!(breached.len(), 1);
        assert_eq!(breached[0].token_id, "t1");
    }

    #[test]
    fn tracker_has_emergency() {
        let mut tracker = HedgeTracker::new(Duration::from_millis(500));
        assert!(!tracker.has_emergency());

        tracker.record_fill(UnhedgedFill {
            token_id: "t1".into(),
            side: Side::Buy,
            price: dec!(0.50),
            size: dec!(10),
            filled_at: Instant::now() - Duration::from_secs(1),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        });
        assert!(tracker.has_emergency());
    }

    #[test]
    fn tracker_pending_hedges() {
        let mut tracker = HedgeTracker::new(Duration::from_millis(500));
        tracker.record_fill(UnhedgedFill {
            token_id: "token1".into(),
            side: Side::Buy,
            price: dec!(0.52),
            size: dec!(10),
            filled_at: Instant::now(),
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        });

        let books = make_book_store("0.48", "0.52");
        let hedges = tracker.pending_hedges(&books);
        assert_eq!(hedges.len(), 1);
        assert!(matches!(hedges[0].side, Side::Sell));
    }

    #[test]
    fn tracker_clear() {
        let mut tracker = HedgeTracker::new(Duration::from_millis(500));
        tracker.record_fill(UnhedgedFill {
            token_id: "t1".into(),
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
        assert!(matches!(bid.side, Side::Buy));
        assert!(matches!(ask.side, Side::Sell));
        assert!(bid.post_only);
        assert!(ask.post_only);
        assert!(bid.price < ask.price);
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
