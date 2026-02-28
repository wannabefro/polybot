
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

/// Generate two-sided quotes for a tradable market using BUY orders only.
///
/// For binary markets (2 tokens): BUY token0 at bid + BUY token1 at complement.
/// We cannot SELL tokens we don't hold — the CLOB rejects with "not enough balance".
pub fn generate_quotes(
    config: &Config,
    market: &TradableMarket,
    books: &BookStore,
    risk: &RiskEngine,
) -> Vec<(OrderIntent, OrderIntent)> {
    if market.tokens.len() < 2 {
        return Vec::new();
    }

    // For binary markets, generate buy/buy-complement pair
    if market.tokens.len() == 2 {
        if let Some(pair) = generate_binary_quotes(config, market, books, risk) {
            return vec![pair];
        }
        return Vec::new();
    }

    // Multi-outcome: just place buy orders on each token
    let mut intents = Vec::new();
    for token in &market.tokens {
        if let Some(intent) = generate_single_buy(config, market, token, books, risk) {
            intents.push((intent.clone(), intent));
        }
    }
    intents
}

/// Binary market: BUY token0 at bid + BUY token1 at complement of ask.
fn generate_binary_quotes(
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
    let mid = (best_bid0 + best_ask0) / Decimal::TWO;
    let spread = best_ask0 - best_bid0;

    if spread <= Decimal::ZERO {
        return None;
    }

    let tick = market.min_tick_size;
    let min_spread = if market.rewards_active {
        tick * dec!(2)
    } else {
        tick * dec!(4)
    };
    let half_spread = (spread / dec!(2)).max(min_spread);

    // Inventory skew
    let inventory = risk.token_inventory(&t0.token_id);
    let skew = if !inventory.is_zero() {
        let raw = -(inventory * dec!(0.001));
        let bound = half_spread / dec!(2);
        raw.clamp(-bound, bound)
    } else {
        Decimal::ZERO
    };

    let mut bid_price = round_to_tick(mid - half_spread + skew, tick);

    // The "ask" on token0 is BUY on token1 at complement price
    let ask_on_t0 = round_to_tick(mid + half_spread + skew, tick);
    let mut complement_price = round_to_tick(Decimal::ONE - ask_on_t0, tick);

    // Enforce max spread
    if let Some(max_spread) = market.rewards_max_spread {
        if (ask_on_t0 - bid_price) > max_spread {
            let tight_half = max_spread / dec!(2);
            bid_price = round_to_tick(mid - tight_half + skew, tick);
            let tight_ask = round_to_tick(mid + tight_half + skew, tick);
            complement_price = round_to_tick(Decimal::ONE - tight_ask, tick);
        }
    }

    // Post-only safety: bid must not cross token0 book
    if bid_price >= best_ask0 {
        bid_price = best_ask0 - tick;
    }
    if bid_price <= Decimal::ZERO {
        return None;
    }

    // Clamp complement against token1 book if available
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

    // Size
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

    if let RiskVerdict::Rejected(reason) = risk.check(&market.condition_id, &bid_intent) {
        debug!(market = %market.question, reason, "rebate-mm: bid rejected");
        return None;
    }
    if let RiskVerdict::Rejected(reason) = risk.check(&market.condition_id, &ask_intent) {
        debug!(market = %market.question, reason, "rebate-mm: complement rejected");
        return None;
    }

    debug!(
        market = %market.question,
        bid = %bid_price,
        complement = %complement_price,
        size = %size,
        "rebate-mm: quoting (buy/buy-complement)"
    );

    Some((bid_intent, ask_intent))
}

/// Single token BUY for multi-outcome markets.
fn generate_single_buy(
    config: &Config,
    market: &TradableMarket,
    token: &crate::market::discovery::TokenInfo,
    books: &BookStore,
    risk: &RiskEngine,
) -> Option<OrderIntent> {
    let book = books.get(&token.token_id)?;
    let best_bid = book.bids.best()?.price;
    let best_ask = book.asks.best()?.price;
    let mid = (best_bid + best_ask) / Decimal::TWO;

    if best_ask <= best_bid {
        return None;
    }

    let tick = market.min_tick_size;
    let mut bid_price = best_bid;

    if bid_price >= best_ask {
        bid_price = best_ask - tick;
    }
    if bid_price <= Decimal::ZERO {
        return None;
    }

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
        debug!(market = %market.question, reason, "rebate-mm: buy rejected");
        return None;
    }

    Some(intent)
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
