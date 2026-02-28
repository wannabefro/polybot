
use polymarket_client_sdk::clob::types::{OrderType, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::debug;

use crate::config::Config;
use crate::market::book::BookStore;
use crate::market::discovery::TradableMarket;
use crate::order::pipeline::OrderIntent;
use crate::risk::guardrails::{RiskEngine, RiskVerdict};

/// Parameters for a single quoting cycle on one market.
#[derive(Debug)]
#[allow(dead_code)]
pub struct QuoteParams {
    pub min_spread: Decimal,
    pub min_size: Decimal,
    pub half_spread: Decimal,
    pub skew_factor: Decimal,
}

/// Generate two-sided quotes for each outcome in a tradable market.
///
/// Returns a Vec of (bid_intent, ask_intent) pairs — one per quotable token.
pub fn generate_quotes(
    config: &Config,
    market: &TradableMarket,
    books: &BookStore,
    risk: &RiskEngine,
) -> Vec<(OrderIntent, OrderIntent)> {
    if market.tokens.len() < 2 {
        return Vec::new();
    }

    let mut pairs = Vec::new();

    for token in &market.tokens {
        if let Some(pair) = generate_token_quotes(config, market, token, books, risk) {
            pairs.push(pair);
        }
    }

    pairs
}

/// Generate bid/ask quotes for a single token within a market.
fn generate_token_quotes(
    config: &Config,
    market: &TradableMarket,
    token: &crate::market::discovery::TokenInfo,
    books: &BookStore,
    risk: &RiskEngine,
) -> Option<(OrderIntent, OrderIntent)> {
    let book = books.get(&token.token_id)?;
    let best_bid = book.bids.best()?.price;
    let best_ask = book.asks.best()?.price;
    let mid = (best_bid + best_ask) / Decimal::TWO;
    let spread = best_ask - best_bid;

    // Ensure we meet rewards constraints
    let min_spread = if market.rewards_active {
        // Use a tighter spread to qualify for rewards
        market.min_tick_size * dec!(2)
    } else {
        market.min_tick_size * dec!(4)
    };

    let half_spread = (spread / dec!(2)).max(min_spread);

    // Inventory skew: shift quotes away from concentrated side
    // Positive inventory (long) → negative skew (lower bid/ask to reduce longs)
    // Negative inventory (short) → positive skew (raise bid/ask to reduce shorts)
    let inventory = risk.token_inventory(&token.token_id);
    let skew = if !inventory.is_zero() {
        let skew_factor = dec!(0.001); // 10 bps per unit of inventory
        let raw = -(inventory * skew_factor);
        let bound = half_spread / dec!(2);
        raw.clamp(-bound, bound)
    } else {
        Decimal::ZERO
    };

    let mut bid_price = round_to_tick(mid - half_spread + skew, market.min_tick_size);
    let mut ask_price = round_to_tick(mid + half_spread + skew, market.min_tick_size);

    // Post-only safety: clamp so we never cross the book
    if bid_price >= best_ask {
        bid_price = best_ask - market.min_tick_size;
    }
    if ask_price <= best_bid {
        ask_price = best_bid + market.min_tick_size;
    }
    if bid_price <= Decimal::ZERO || ask_price <= bid_price {
        return None;
    }

    // Size: use min_order_size, rewards min_size, or base minimum — whichever is largest
    let rewards_min = market.rewards_min_size.unwrap_or(Decimal::ZERO);
    let mm_min = Decimal::from_f64_retain(config.mm_min_size).unwrap_or(Decimal::ZERO);
    let mut size = market.min_order_size.max(rewards_min).max(mm_min);
    let inventory_cap_notional = Decimal::from_f64_retain(
        config.nav_limit(config.effective_max_one_sided_inventory())
    ).unwrap_or(Decimal::ZERO);
    if !mid.is_zero() && inventory_cap_notional > Decimal::ZERO {
        size = size.min(inventory_cap_notional / mid);
    }
    if size < market.min_order_size {
        return None;
    }

    // Enforce rewards max spread constraint
    if let Some(max_spread) = market.rewards_max_spread {
        if (ask_price - bid_price) > max_spread {
            debug!(
                market = %market.question,
                spread = %(ask_price - bid_price),
                max = %max_spread,
                "rebate-mm: spread exceeds rewards max_spread, tightening"
            );
            // Tighten to fit within rewards constraint
            let tight_half = max_spread / dec!(2);
            let bid_price = round_to_tick(mid - tight_half + skew, market.min_tick_size);
            let ask_price = round_to_tick(mid + tight_half + skew, market.min_tick_size);
            if ask_price <= bid_price {
                return None;
            }
            // Re-create intents with tightened prices
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
            if let RiskVerdict::Rejected(reason) = risk.check(&market.condition_id, &bid_intent) {
                debug!(market = %market.question, reason, "rebate-mm: tightened bid rejected");
                return None;
            }
            if let RiskVerdict::Rejected(reason) = risk.check(&market.condition_id, &ask_intent) {
                debug!(market = %market.question, reason, "rebate-mm: tightened ask rejected");
                return None;
            }
            debug!(
                market = %market.question,
                bid = %bid_price,
                ask = %ask_price,
                size = %size,
                skew = %skew,
                "rebate-mm: quoting (tightened)"
            );
            return Some((bid_intent, ask_intent));
        }
    }

    // Risk check
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

    // Pre-trade risk checks
    if let RiskVerdict::Rejected(reason) = risk.check(&market.condition_id, &bid_intent) {
        debug!(market = %market.question, reason, "rebate-mm: bid rejected by risk");
        return None;
    }
    if let RiskVerdict::Rejected(reason) = risk.check(&market.condition_id, &ask_intent) {
        debug!(market = %market.question, reason, "rebate-mm: ask rejected by risk");
        return None;
    }

    debug!(
        market = %market.question,
        bid = %bid_price,
        ask = %ask_price,
        size = %size,
        "rebate-mm: quoting"
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

    #[test]
    fn round_to_tick_works() {
        assert_eq!(round_to_tick(dec!(0.523), dec!(0.01)), dec!(0.52));
        assert_eq!(round_to_tick(dec!(0.526), dec!(0.01)), dec!(0.53));
        assert_eq!(round_to_tick(dec!(0.50), dec!(0.001)), dec!(0.500));
    }
}
