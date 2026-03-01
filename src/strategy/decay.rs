//! Late-stage time-decay strategy ("Penny Collector").
//!
//! Scans for binary markets resolving within a configurable window (default 24h)
//! where one outcome trades at >= min_price (default 0.90). Buys shares via IOC
//! and holds to resolution. Strict per-market and total capital limits.

use chrono::{DateTime, Utc};
use polymarket_client_sdk::clob::types::{OrderType, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use tracing::{debug, info};

use crate::config::Config;
use crate::market::book::BookStore;
use crate::market::discovery::TradableMarket;
use crate::order::pipeline::OrderIntent;

/// Tracks deployed capital and active decay positions.
#[derive(Debug)]
pub struct DecayTracker {
    /// condition_id → (token_id, size bought, cost_basis)
    pub positions: HashMap<String, DecayPosition>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DecayPosition {
    pub token_id: String,
    pub condition_id: String,
    pub outcome: String,
    pub size: Decimal,
    pub cost_basis: Decimal,
    pub neg_risk: bool,
    pub fee_rate_bps: Decimal,
}

impl DecayTracker {
    pub fn new() -> Self {
        Self {
            positions: HashMap::new(),
        }
    }

    /// Total USDC currently deployed in decay positions.
    pub fn deployed_capital(&self) -> Decimal {
        self.positions.values().map(|p| p.cost_basis).sum()
    }

    /// Record a fill from a decay buy.
    pub fn record_fill(
        &mut self,
        condition_id: &str,
        token_id: &str,
        outcome: &str,
        size: Decimal,
        price: Decimal,
        neg_risk: bool,
        fee_rate_bps: Decimal,
    ) {
        let cost = size * price;
        let entry = self
            .positions
            .entry(condition_id.to_string())
            .or_insert_with(|| DecayPosition {
                token_id: token_id.to_string(),
                condition_id: condition_id.to_string(),
                outcome: outcome.to_string(),
                size: Decimal::ZERO,
                cost_basis: Decimal::ZERO,
                neg_risk,
                fee_rate_bps,
            });
        entry.size += size;
        entry.cost_basis += cost;
    }

    /// Remove positions for markets that are no longer accepting orders (resolved).
    pub fn cleanup_resolved(&mut self, markets: &[TradableMarket]) {
        let active_ids: std::collections::HashSet<&str> =
            markets.iter().map(|m| m.condition_id.as_str()).collect();
        let before = self.positions.len();
        self.positions.retain(|cid, _| active_ids.contains(cid.as_str()));
        let removed = before - self.positions.len();
        if removed > 0 {
            info!(removed, "decay: cleaned up resolved positions");
        }
    }

    /// Number of active positions.
    pub fn position_count(&self) -> usize {
        self.positions.len()
    }
}

/// A candidate market for a decay buy.
#[derive(Debug)]
pub struct DecayCandidate {
    pub condition_id: String,
    pub token_id: String,
    pub outcome: String,
    pub price: Decimal,
    pub hours_to_end: f64,
    pub neg_risk: bool,
    pub fee_rate_bps: Decimal,
    pub min_order_size: Decimal,
}

/// Scan tradable markets for decay candidates.
pub fn scan_candidates(
    markets: &[TradableMarket],
    books: &BookStore,
    config: &Config,
    now: DateTime<Utc>,
) -> Vec<DecayCandidate> {
    if !config.decay_enabled {
        return vec![];
    }

    let min_price = Decimal::try_from(config.decay_min_price).unwrap_or(dec!(0.93));
    let window_secs = (config.decay_window_hours * 3600.0) as i64;

    let mut candidates = Vec::new();

    for market in markets {
        // Binary markets only (exactly 2 outcomes)
        if market.tokens.len() != 2 {
            continue;
        }

        // Must have an end date within the window
        let end_date = match market.end_date {
            Some(d) => d,
            None => continue,
        };
        let secs_remaining = (end_date - now).num_seconds();
        if secs_remaining <= 0 || secs_remaining > window_secs {
            continue;
        }

        // Tag exclusion (case-insensitive)
        let excluded = market.tags.iter().any(|t| {
            let lower = t.to_lowercase();
            config.decay_excluded_tags.iter().any(|ex| lower.contains(ex))
        });
        if excluded {
            continue;
        }

        // Find the highest-priced token that exceeds min_price
        let best_token = market
            .tokens
            .iter()
            .filter(|t| t.price >= min_price)
            .max_by_key(|t| t.price);

        let token = match best_token {
            Some(t) => t,
            None => continue,
        };

        // Verify there's actually an ask available at a reasonable price
        let book = match books.get(&token.token_id) {
            Some(b) => b,
            None => continue,
        };
        let best_ask = match book.asks.best() {
            Some(a) => a.price,
            None => continue,
        };
        if best_ask > Decimal::ONE || best_ask < min_price {
            continue;
        }

        let hours_to_end = secs_remaining as f64 / 3600.0;

        candidates.push(DecayCandidate {
            condition_id: market.condition_id.clone(),
            token_id: token.token_id.clone(),
            outcome: token.outcome.clone(),
            price: best_ask,
            hours_to_end,
            neg_risk: market.neg_risk,
            fee_rate_bps: market.maker_fee_bps,
            min_order_size: market.min_order_size,
        });
    }

    // Sort by price descending (highest conviction first)
    candidates.sort_by(|a, b| b.price.partial_cmp(&a.price).unwrap_or(std::cmp::Ordering::Equal));
    candidates
}

/// Build an IOC buy intent for a decay candidate with strict sizing.
pub fn evaluate_decay_buy(
    candidate: &DecayCandidate,
    config: &Config,
    tracker: &DecayTracker,
) -> Option<OrderIntent> {
    let max_bet = Decimal::try_from(config.decay_max_bet_usdc).unwrap_or(dec!(2.0));
    let nav_cap = Decimal::try_from(config.nav_usdc * config.decay_nav_fraction)
        .unwrap_or(dec!(100.0));

    // Check total capital deployed
    let deployed = tracker.deployed_capital();
    let remaining_budget = nav_cap - deployed;
    if remaining_budget <= Decimal::ZERO {
        debug!(deployed = %deployed, nav_cap = %nav_cap, "decay: capital budget exhausted");
        return None;
    }

    // Already have a position in this market?
    if tracker.positions.contains_key(&candidate.condition_id) {
        return None;
    }

    // Per-market cap
    let usdc_to_spend = max_bet.min(remaining_budget);

    // Calculate size: spend / price (how many shares we get)
    let size = (usdc_to_spend / candidate.price)
        .round_dp_with_strategy(2, rust_decimal::RoundingStrategy::ToZero);

    if size < candidate.min_order_size || size <= Decimal::ZERO {
        return None;
    }

    debug!(
        condition_id = %candidate.condition_id,
        outcome = %candidate.outcome,
        price = %candidate.price,
        size = %size,
        hours_to_end = candidate.hours_to_end,
        "decay: buy candidate"
    );

    Some(OrderIntent {
        token_id: candidate.token_id.clone(),
        side: Side::Buy,
        price: candidate.price,
        size,
        order_type: OrderType::FOK,
        post_only: false,
        neg_risk: candidate.neg_risk,
        fee_rate_bps: candidate.fee_rate_bps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use crate::market::book::BookStore;
    use crate::market::discovery::{TradableMarket, TokenInfo};
    use chrono::Duration as ChronoDuration;

    fn make_market(
        condition_id: &str,
        end_date: Option<DateTime<Utc>>,
        tokens: Vec<TokenInfo>,
        tags: Vec<String>,
        neg_risk: bool,
    ) -> TradableMarket {
        TradableMarket {
            condition_id: condition_id.to_string(),
            question: "Test market?".to_string(),
            tokens,
            neg_risk,
            neg_risk_market_id: None,
            min_tick_size: dec!(0.01),
            min_order_size: dec!(1.0),
            maker_fee_bps: dec!(0),
            rewards_active: false,
            rewards_max_spread: None,
            rewards_min_size: None,
            volume_24h: 0.0,
            tags,
            end_date,
        }
    }

    fn make_tokens(yes_price: &str, no_price: &str) -> Vec<TokenInfo> {
        vec![
            TokenInfo {
                token_id: "tok_yes".to_string(),
                outcome: "Yes".to_string(),
                price: yes_price.parse().unwrap(),
            },
            TokenInfo {
                token_id: "tok_no".to_string(),
                outcome: "No".to_string(),
                price: no_price.parse().unwrap(),
            },
        ]
    }

    fn make_books_with_ask(token_id: &str, ask_price: Decimal) -> BookStore {
        use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, OrderBookLevel};
        use polymarket_client_sdk::types::{B256, U256};
        let books = BookStore::new();
        let level = OrderBookLevel::builder()
            .price(ask_price)
            .size(dec!(100))
            .build();
        let update = BookUpdate::builder()
            .asset_id(U256::ZERO)
            .market(B256::ZERO)
            .timestamp(1000)
            .bids(vec![])
            .asks(vec![level])
            .hash("test".into())
            .build();
        books.apply(token_id, &update);
        books
    }

    #[test]
    fn scan_finds_candidate_within_window() {
        let now = Utc::now();
        let end = now + ChronoDuration::hours(12);
        let market = make_market("c1", Some(end), make_tokens("0.95", "0.05"), vec![], false);
        let books = make_books_with_ask("tok_yes", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].outcome, "Yes");
        assert_eq!(candidates[0].price, dec!(0.95));
    }

    #[test]
    fn scan_skips_market_outside_window() {
        let now = Utc::now();
        let end = now + ChronoDuration::hours(96); // beyond 72h window
        let market = make_market("c1", Some(end), make_tokens("0.95", "0.05"), vec![], false);
        let books = make_books_with_ask("tok_yes", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert!(candidates.is_empty());
    }

    #[test]
    fn scan_skips_low_price() {
        let now = Utc::now();
        let end = now + ChronoDuration::hours(12);
        let market = make_market("c1", Some(end), make_tokens("0.80", "0.20"), vec![], false);
        let books = make_books_with_ask("tok_yes", dec!(0.80));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert!(candidates.is_empty());
    }

    #[test]
    fn scan_skips_excluded_tags() {
        let now = Utc::now();
        let end = now + ChronoDuration::hours(12);
        let market = make_market(
            "c1", Some(end), make_tokens("0.95", "0.05"),
            vec!["Crypto".to_string()], false,
        );
        let books = make_books_with_ask("tok_yes", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert!(candidates.is_empty());
    }

    #[test]
    fn scan_accepts_neg_risk() {
        let now = Utc::now();
        let end = now + ChronoDuration::hours(12);
        let market = make_market("c1", Some(end), make_tokens("0.95", "0.05"), vec![], true);
        let books = make_books_with_ask("tok_yes", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert_eq!(candidates.len(), 1);
    }

    #[test]
    fn scan_skips_no_end_date() {
        let now = Utc::now();
        let market = make_market("c1", None, make_tokens("0.95", "0.05"), vec![], false);
        let books = make_books_with_ask("tok_yes", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert!(candidates.is_empty());
    }

    #[test]
    fn scan_skips_multi_outcome() {
        let now = Utc::now();
        let end = now + ChronoDuration::hours(12);
        let tokens = vec![
            TokenInfo { token_id: "t1".into(), outcome: "A".into(), price: dec!(0.95) },
            TokenInfo { token_id: "t2".into(), outcome: "B".into(), price: dec!(0.03) },
            TokenInfo { token_id: "t3".into(), outcome: "C".into(), price: dec!(0.02) },
        ];
        let market = make_market("c1", Some(end), tokens, vec![], false);
        let books = make_books_with_ask("t1", dec!(0.95));
        let config = test_config();

        let candidates = scan_candidates(&[market], &books, &config, now);
        assert!(candidates.is_empty());
    }

    #[test]
    fn evaluate_respects_max_bet() {
        let config = test_config();
        let tracker = DecayTracker::new();
        let candidate = DecayCandidate {
            condition_id: "c1".to_string(),
            token_id: "tok_yes".to_string(),
            outcome: "Yes".to_string(),
            price: dec!(0.95),
            hours_to_end: 12.0,
            neg_risk: false,
            fee_rate_bps: dec!(0),
            min_order_size: dec!(1.0),
        };

        let intent = evaluate_decay_buy(&candidate, &config, &tracker).unwrap();
        // $10 / $0.95 = 10.52 shares (rounded down to 10.52)
        assert!(intent.size <= dec!(10.53));
        assert!(intent.size >= dec!(10.0));
        assert_eq!(intent.order_type, OrderType::FOK);
    }

    #[test]
    fn evaluate_skips_duplicate_market() {
        let config = test_config();
        let mut tracker = DecayTracker::new();
        tracker.record_fill("c1", "tok_yes", "Yes", dec!(2), dec!(0.95), false, dec!(0));

        let candidate = DecayCandidate {
            condition_id: "c1".to_string(),
            token_id: "tok_yes".to_string(),
            outcome: "Yes".to_string(),
            price: dec!(0.95),
            hours_to_end: 12.0,
            neg_risk: false,
            fee_rate_bps: dec!(0),
            min_order_size: dec!(1.0),
        };

        assert!(evaluate_decay_buy(&candidate, &config, &tracker).is_none());
    }

    #[test]
    fn evaluate_respects_nav_cap() {
        let mut config = test_config();
        config.nav_usdc = 100.0;
        config.decay_nav_fraction = 0.50; // $50 budget
        config.decay_max_bet_usdc = 2.0;

        let mut tracker = DecayTracker::new();
        // Deploy $50 already
        tracker.record_fill("c_old", "t_old", "Yes", dec!(52), dec!(0.96), false, dec!(0));

        let candidate = DecayCandidate {
            condition_id: "c1".to_string(),
            token_id: "tok_yes".to_string(),
            outcome: "Yes".to_string(),
            price: dec!(0.95),
            hours_to_end: 12.0,
            neg_risk: false,
            fee_rate_bps: dec!(0),
            min_order_size: dec!(1.0),
        };

        assert!(evaluate_decay_buy(&candidate, &config, &tracker).is_none());
    }

    #[test]
    fn tracker_cleanup_removes_resolved() {
        let mut tracker = DecayTracker::new();
        tracker.record_fill("c1", "t1", "Yes", dec!(2), dec!(0.95), false, dec!(0));
        tracker.record_fill("c2", "t2", "Yes", dec!(2), dec!(0.95), false, dec!(0));

        // Only c1 is still active
        let markets = vec![make_market(
            "c1", None, make_tokens("0.95", "0.05"), vec![], false,
        )];
        tracker.cleanup_resolved(&markets);
        assert_eq!(tracker.position_count(), 1);
        assert!(tracker.positions.contains_key("c1"));
    }

    #[test]
    fn candidates_sorted_by_price_descending() {
        let now = Utc::now();
        let end = now + ChronoDuration::hours(12);

        let m1 = make_market("c1", Some(end), vec![
            TokenInfo { token_id: "t1".into(), outcome: "Yes".into(), price: dec!(0.93) },
            TokenInfo { token_id: "t1n".into(), outcome: "No".into(), price: dec!(0.07) },
        ], vec![], false);
        let m2 = make_market("c2", Some(end), vec![
            TokenInfo { token_id: "t2".into(), outcome: "Yes".into(), price: dec!(0.97) },
            TokenInfo { token_id: "t2n".into(), outcome: "No".into(), price: dec!(0.03) },
        ], vec![], false);

        let books = BookStore::new();
        {
            use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, OrderBookLevel};
            use polymarket_client_sdk::types::{B256, U256};
            for (tid, ask) in [("t1", dec!(0.93)), ("t2", dec!(0.97))] {
                let level = OrderBookLevel::builder()
                    .price(ask)
                    .size(dec!(100))
                    .build();
                let update = BookUpdate::builder()
                    .asset_id(U256::ZERO)
                    .market(B256::ZERO)
                    .timestamp(1000)
                    .bids(vec![])
                    .asks(vec![level])
                    .hash("test".into())
                    .build();
                books.apply(tid, &update);
            }
        }

        let config = test_config();
        let candidates = scan_candidates(&[m1, m2], &books, &config, now);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].price, dec!(0.97)); // highest first
        assert_eq!(candidates[1].price, dec!(0.93));
    }
}
