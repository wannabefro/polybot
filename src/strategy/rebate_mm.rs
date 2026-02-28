use std::sync::Arc;

use polymarket_client_sdk::clob::types::{OrderType, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::{debug, info};

use crate::config::Config;
use crate::market::book::BookStore;
use crate::market::discovery::TradableMarket;
use crate::order::pipeline::OrderIntent;
use crate::risk::guardrails::{RiskEngine, RiskVerdict};

/// Parameters for a single quoting cycle on one market.
#[derive(Debug)]
pub struct QuoteParams {
    pub min_spread: Decimal,
    pub min_size: Decimal,
    pub half_spread: Decimal,
    pub skew_factor: Decimal,
}

/// Generate two-sided quotes for a tradable market.
///
/// Returns (bid_intent, ask_intent) or None if the market isn't quotable.
pub fn generate_quotes(
    config: &Config,
    market: &TradableMarket,
    books: &BookStore,
    risk: &RiskEngine,
) -> Option<(OrderIntent, OrderIntent)> {
    if market.tokens.len() < 2 {
        return None;
    }

    let token = &market.tokens[0];
    let book = books.get(&token.token_id)?;
    let mid = book.mid_price()?;
    let spread = book.spread()?;

    // Ensure we meet rewards constraints
    let min_spread = if market.rewards_active {
        // Use a tighter spread to qualify for rewards
        market.min_tick_size * dec!(2)
    } else {
        market.min_tick_size * dec!(4)
    };

    let half_spread = (spread / dec!(2)).max(min_spread);

    // Inventory skew: if we're long, lower our bid (less eager to buy more)
    // This is a simple placeholder; production would use actual position data
    let skew = Decimal::ZERO;

    let bid_price = round_to_tick(mid - half_spread + skew, market.min_tick_size);
    let ask_price = round_to_tick(mid + half_spread + skew, market.min_tick_size);

    // Size: use min_order_size or rewards min_size, whichever is larger
    let size = market.min_order_size.max(dec!(5)); // 5 shares minimum

    // Risk check
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

    // Pre-trade risk checks
    if let RiskVerdict::Rejected(reason) = risk.check(&market.condition_id, &bid_intent) {
        debug!(market = %market.question, reason, "rebate-mm: bid rejected by risk");
        return None;
    }
    if let RiskVerdict::Rejected(reason) = risk.check(&market.condition_id, &ask_intent) {
        debug!(market = %market.question, reason, "rebate-mm: ask rejected by risk");
        return None;
    }

    info!(
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
