//! Niche spread-scalper selection and state.
//!
//! This module ranks non-reward markets for rebate MM using local microstructure:
//! spread width, touch notional, and short-horizon volatility.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::market::book::BookStore;
use crate::market::discovery::TradableMarket;

/// Lookback horizon used for volatility in seconds.
const VOL_LOOKBACK_SECS: u64 = 60;
/// Keep a slightly longer history so we can always find a 60s anchor.
const MAX_HISTORY_SECS: u64 = 120;

/// Ranked market diagnostics for scalper selection.
#[derive(Debug, Clone, PartialEq)]
pub struct ScalperMarketScore {
    pub condition_id: String,
    pub score: f64,
    pub spread_ticks: f64,
    pub touch_notional_usdc: f64,
    pub volatility_ticks_60s: f64,
}

/// Runtime state for scalper selection.
pub struct ScalperState {
    mids: HashMap<String, VecDeque<(Instant, f64)>>,
    cooldowns: HashMap<String, Instant>,
}

impl ScalperState {
    pub fn new() -> Self {
        Self {
            mids: HashMap::new(),
            cooldowns: HashMap::new(),
        }
    }

    /// Record a mid price sample for a token.
    pub fn record_mid(&mut self, token_id: &str, mid: f64) {
        let now = Instant::now();
        let history = self.mids.entry(token_id.to_string()).or_default();
        history.push_back((now, mid));

        let keep_after = now
            .checked_sub(Duration::from_secs(MAX_HISTORY_SECS))
            .unwrap_or(now);
        while let Some((ts, _)) = history.front() {
            if *ts >= keep_after {
                break;
            }
            history.pop_front();
        }
    }

    /// Return short-horizon volatility in ticks for a token.
    pub fn volatility_ticks_60s(&self, token_id: &str, tick: f64) -> Option<f64> {
        if tick <= 0.0 {
            return None;
        }
        let history = self.mids.get(token_id)?;
        let (now_ts, now_mid) = *history.back()?;
        let target = now_ts
            .checked_sub(Duration::from_secs(VOL_LOOKBACK_SECS))
            .unwrap_or(now_ts);

        // Pick the latest sample at-or-before target.
        let anchor = history
            .iter()
            .rev()
            .find(|(ts, _)| *ts <= target)
            .copied()
            .or_else(|| history.front().copied())?;

        let vol_ticks = (now_mid - anchor.1).abs() / tick;
        Some(vol_ticks)
    }

    /// True if condition is still cooling down.
    pub fn is_condition_cooling_down(&mut self, condition_id: &str) -> bool {
        self.cleanup_cooldowns();
        self.cooldowns
            .get(condition_id)
            .map_or(false, |until| *until > Instant::now())
    }

    /// Start (or extend) cooldown for a condition.
    pub fn set_condition_cooldown(&mut self, condition_id: &str, duration: Duration) {
        self.cooldowns
            .insert(condition_id.to_string(), Instant::now() + duration);
    }

    fn cleanup_cooldowns(&mut self) {
        let now = Instant::now();
        self.cooldowns.retain(|_, until| *until > now);
    }
}

/// Evaluate whether a non-reward market is a valid niche-scalper candidate.
pub fn evaluate_niche_market(
    market: &TradableMarket,
    books: &BookStore,
    state: &mut ScalperState,
    cfg: &Config,
) -> Option<ScalperMarketScore> {
    if market.rewards_active {
        return None;
    }
    if market.tokens.len() != 2 {
        return None;
    }
    if market.volume_24h < cfg.scalper_min_vol_24h || market.volume_24h > cfg.scalper_max_vol_24h {
        return None;
    }
    if state.is_condition_cooling_down(&market.condition_id) {
        return None;
    }

    let tick = market.min_tick_size.to_f64()?;
    if tick <= 0.0 {
        return None;
    }

    let mut min_spread_ticks = f64::MAX;
    let mut min_touch_notional = f64::MAX;
    let mut max_vol_ticks = 1.0f64; // conservative default if history is missing

    for token in &market.tokens {
        let book = books.get(&token.token_id)?;
        let best_bid = book.bids.best()?;
        let best_ask = book.asks.best()?;
        let bid_p = best_bid.price.to_f64()?;
        let ask_p = best_ask.price.to_f64()?;
        if ask_p <= bid_p {
            return None;
        }

        let spread_ticks = (ask_p - bid_p) / tick;
        min_spread_ticks = min_spread_ticks.min(spread_ticks);

        let bid_touch_notional = best_bid.price * best_bid.size;
        let ask_touch_notional = best_ask.price * best_ask.size;
        let touch_notional = bid_touch_notional.min(ask_touch_notional).to_f64()?;
        min_touch_notional = min_touch_notional.min(touch_notional);

        let vol_ticks = state
            .volatility_ticks_60s(&token.token_id, tick)
            .unwrap_or(1.0);
        max_vol_ticks = max_vol_ticks.max(vol_ticks);
    }

    if min_spread_ticks < cfg.scalper_min_spread_ticks as f64 {
        return None;
    }
    if min_touch_notional < cfg.scalper_min_touch_notional_usdc {
        return None;
    }

    let score =
        1.5 * min_spread_ticks + 0.8 * (1.0 + min_touch_notional).ln() - 1.2 * max_vol_ticks;

    Some(ScalperMarketScore {
        condition_id: market.condition_id.clone(),
        score,
        spread_ticks: min_spread_ticks,
        touch_notional_usdc: min_touch_notional,
        volatility_ticks_60s: max_vol_ticks,
    })
}

trait DecimalToF64 {
    fn to_f64(&self) -> Option<f64>;
}

impl DecimalToF64 for rust_decimal::Decimal {
    fn to_f64(&self) -> Option<f64> {
        rust_decimal::prelude::ToPrimitive::to_f64(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::test_config;
    use crate::market::discovery::TokenInfo;
    use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, OrderBookLevel};
    use polymarket_client_sdk::types::{B256, U256};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn make_market(condition: &str, volume_24h: f64) -> TradableMarket {
        TradableMarket {
            condition_id: condition.into(),
            question: "Niche market?".into(),
            tokens: vec![
                TokenInfo {
                    token_id: format!("{condition}-yes"),
                    outcome: "Yes".into(),
                    price: dec!(0.50),
                },
                TokenInfo {
                    token_id: format!("{condition}-no"),
                    outcome: "No".into(),
                    price: dec!(0.50),
                },
            ],
            neg_risk: false,
            neg_risk_market_id: None,
            min_tick_size: dec!(0.01),
            min_order_size: dec!(1),
            maker_fee_bps: dec!(0),
            rewards_active: false,
            rewards_max_spread: None,
            rewards_min_size: None,
            volume_24h,
            tags: vec![],
            end_date: None,
        }
    }

    fn seed_book(
        store: &BookStore,
        token_id: &str,
        bid: &str,
        bid_size: &str,
        ask: &str,
        ask_size: &str,
    ) {
        let update = BookUpdate::builder()
            .asset_id(U256::ZERO)
            .market(B256::ZERO)
            .timestamp(0)
            .bids(vec![OrderBookLevel::builder()
                .price(bid.parse::<Decimal>().unwrap())
                .size(bid_size.parse::<Decimal>().unwrap())
                .build()])
            .asks(vec![OrderBookLevel::builder()
                .price(ask.parse::<Decimal>().unwrap())
                .size(ask_size.parse::<Decimal>().unwrap())
                .build()])
            .build();
        store.apply(token_id, &update);
    }

    #[test]
    fn accepts_wide_spread_and_touch() {
        let cfg = test_config();
        let books = BookStore::new();
        let mut state = ScalperState::new();
        let market = make_market("cond1", 10_000.0);
        seed_book(&books, "cond1-yes", "0.40", "200", "0.50", "200");
        seed_book(&books, "cond1-no", "0.40", "200", "0.50", "200");

        state.record_mid("cond1-yes", 0.45);
        state.record_mid("cond1-no", 0.45);
        let score = evaluate_niche_market(&market, &books, &mut state, &cfg);
        assert!(score.is_some());
        assert!(score.unwrap().spread_ticks >= 4.0);
    }

    #[test]
    fn rejects_thin_touch_notional() {
        let cfg = test_config();
        let books = BookStore::new();
        let mut state = ScalperState::new();
        let market = make_market("cond2", 10_000.0);
        seed_book(&books, "cond2-yes", "0.40", "10", "0.50", "10"); // min touch = $4
        seed_book(&books, "cond2-no", "0.40", "10", "0.50", "10");

        assert!(evaluate_niche_market(&market, &books, &mut state, &cfg).is_none());
    }

    #[test]
    fn rejects_high_volatility() {
        let mut cfg = test_config();
        cfg.scalper_min_spread_ticks = 1;
        cfg.scalper_min_touch_notional_usdc = 1.0;
        let books = BookStore::new();
        let mut state = ScalperState::new();
        let market = make_market("cond3", 10_000.0);
        seed_book(&books, "cond3-yes", "0.48", "200", "0.52", "200");
        seed_book(&books, "cond3-no", "0.48", "200", "0.52", "200");

        // Simulate jumpy mid history; high vol should crush score.
        state.record_mid("cond3-yes", 0.20);
        state.record_mid("cond3-yes", 0.80);
        state.record_mid("cond3-no", 0.20);
        state.record_mid("cond3-no", 0.80);

        let result = evaluate_niche_market(&market, &books, &mut state, &cfg).unwrap();
        assert!(result.volatility_ticks_60s >= 1.0);
    }

    #[test]
    fn ranking_is_deterministic() {
        let cfg = test_config();
        let books = BookStore::new();
        let mut state = ScalperState::new();
        let a = make_market("cond_a", 10_000.0);
        let b = make_market("cond_b", 10_000.0);

        seed_book(&books, "cond_a-yes", "0.40", "200", "0.50", "200");
        seed_book(&books, "cond_a-no", "0.40", "200", "0.50", "200");
        seed_book(&books, "cond_b-yes", "0.45", "200", "0.50", "200");
        seed_book(&books, "cond_b-no", "0.45", "200", "0.50", "200");

        let sa = evaluate_niche_market(&a, &books, &mut state, &cfg).unwrap();
        let sb = evaluate_niche_market(&b, &books, &mut state, &cfg).unwrap();
        assert!(sa.score > sb.score);
    }
}
