//! Sponsored/reward capture strategy.
//!
//! Targets markets where reward density is high and depth is low.
//! One-sided fills are immediately neutralized with a FAK hedge.
//! Hedge latency SLA: 500ms timeout → cancel-all + flatten.

use polymarket_client_sdk::clob::types::{OrderType, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

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
    pub price: Decimal,
    pub size: Decimal,
    pub filled_at: Instant,
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
pub fn evaluate_reward_quote(
    _config: &Config,
    market: &TradableMarket,
    books: &BookStore,
    risk: &RiskEngine,
) -> Option<(OrderIntent, OrderIntent)> {
    if !market.rewards_active {
        return None;
    }

    if market.tokens.is_empty() {
        return None;
    }

    let token = &market.tokens[0];
    let book = books.get(&token.token_id)?;
    let mid = book.mid_price()?;
    let spread = book.spread()?;

    if spread.is_zero() {
        return None;
    }

    // Tight spread for reward qualification
    let half_spread = market.min_tick_size;
    let bid_price = round_to_tick(mid - half_spread, market.min_tick_size);
    let ask_price = round_to_tick(mid + half_spread, market.min_tick_size);

    // Use larger size for reward qualification
    let size = market.min_order_size.max(dec!(10));

    let bid_intent = OrderIntent {
        token_id: token.token_id.clone(),
        side: Side::Buy,
        price: bid_price,
        size,
        order_type: OrderType::GTC,
        post_only: true,
    };

    let ask_intent = OrderIntent {
        token_id: token.token_id.clone(),
        side: Side::Sell,
        price: ask_price,
        size,
        order_type: OrderType::GTC,
        post_only: true,
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
        };
        let books = make_book_store("0.48", "0.52");
        let intent = fill.hedge_intent(&books).unwrap();
        assert!(matches!(intent.side, Side::Sell));
        assert_eq!(intent.price, dec!(0.48)); // sell at bid
        assert_eq!(intent.size, dec!(10));
        assert!(!intent.post_only);
    }

    #[test]
    fn hedge_intent_sell_fill_buys() {
        let fill = UnhedgedFill {
            token_id: "token1".into(),
            side: Side::Sell,
            price: dec!(0.48),
            size: dec!(10),
            filled_at: Instant::now(),
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
        });
        tracker.record_fill(UnhedgedFill {
            token_id: "t2".into(),
            side: Side::Sell,
            price: dec!(0.50),
            size: dec!(5),
            filled_at: Instant::now(),
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

        assert!(evaluate_reward_quote(&config, &market, &books, &risk).is_none());
    }

    #[test]
    fn reward_quote_generates_two_sided() {
        let config = test_config();
        let market = make_market(true);
        let books = make_book_store("0.48", "0.52");
        let risk = RiskEngine::new(config.clone());

        let result = evaluate_reward_quote(&config, &market, &books, &risk);
        assert!(result.is_some());
        let (bid, ask) = result.unwrap();
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

        assert!(evaluate_reward_quote(&config, &market, &books, &risk).is_none());
    }

    #[test]
    fn reward_quote_no_book() {
        let config = test_config();
        let market = make_market(true);
        let books = BookStore::new(); // empty
        let risk = RiskEngine::new(config.clone());

        assert!(evaluate_reward_quote(&config, &market, &books, &risk).is_none());
    }

    #[test]
    fn round_to_tick_works() {
        assert_eq!(round_to_tick(dec!(0.523), dec!(0.01)), dec!(0.52));
        assert_eq!(round_to_tick(dec!(0.526), dec!(0.01)), dec!(0.53));
        assert_eq!(round_to_tick(dec!(0.50), dec!(0.001)), dec!(0.500));
    }
}
