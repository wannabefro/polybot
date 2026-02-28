use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use rust_decimal::Decimal;
use tracing::info;

use crate::market::book::BookStore;
use crate::order::pipeline::OrderIntent;

/// Simulated fill for paper trading.
#[derive(Debug, Clone)]
pub struct PaperFill {
    pub paper_order_id: String,
    pub token_id: String,
    pub side: String,
    pub price: Decimal,
    pub size: Decimal,
    pub notional: Decimal,
}

/// Paper trading engine — simulates order placement against the local book.
#[derive(Debug)]
pub struct PaperEngine {
    next_id: RwLock<u64>,
    fills: RwLock<Vec<PaperFill>>,
    open_orders: RwLock<HashMap<String, OrderIntent>>,
}

impl PaperEngine {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            next_id: RwLock::new(1),
            fills: RwLock::new(Vec::new()),
            open_orders: RwLock::new(HashMap::new()),
        })
    }

    /// Simulate placing an order. Returns (order_id, optional_fill).
    ///
    /// If the book has a matching price level, we simulate an immediate fill.
    /// Otherwise, the order rests as an open order.
    pub fn place_order(
        &self,
        intent: &OrderIntent,
        books: &BookStore,
    ) -> (String, Option<PaperFill>) {
        let mut id_guard = self.next_id.write();
        let order_id = format!("paper-{}", *id_guard);
        *id_guard += 1;
        drop(id_guard);

        let notional = intent.price * intent.size;

        // Check if the order would be immediately filled (crosses the book)
        let would_fill = if let Some(book) = books.get(&intent.token_id) {
            match intent.side {
                polymarket_client_sdk::clob::types::Side::Buy => {
                    book.asks.best().map_or(false, |ask| intent.price >= ask.price)
                }
                polymarket_client_sdk::clob::types::Side::Sell => {
                    book.bids.best().map_or(false, |bid| intent.price <= bid.price)
                }
                _ => false,
            }
        } else {
            false
        };

        if would_fill && !intent.post_only {
            let fill = PaperFill {
                paper_order_id: order_id.clone(),
                token_id: intent.token_id.clone(),
                side: format!("{:?}", intent.side),
                price: intent.price,
                size: intent.size,
                notional,
            };
            info!(
                id = %order_id,
                side = %fill.side,
                price = %fill.price,
                size = %fill.size,
                "paper: filled"
            );
            self.fills.write().push(fill.clone());
            (order_id, Some(fill))
        } else {
            info!(
                id = %order_id,
                side = ?intent.side,
                price = %intent.price,
                size = %intent.size,
                post_only = intent.post_only,
                "paper: order resting"
            );
            self.open_orders.write().insert(order_id.clone(), intent.clone());
            (order_id, None)
        }
    }

    /// Cancel a paper order.
    pub fn cancel_order(&self, order_id: &str) -> bool {
        self.open_orders.write().remove(order_id).is_some()
    }

    /// Cancel all paper orders.
    pub fn cancel_all(&self) -> usize {
        let mut orders = self.open_orders.write();
        let count = orders.len();
        orders.clear();
        info!(count, "paper: cancel-all");
        count
    }

    /// Get all fills.
    pub fn fills(&self) -> Vec<PaperFill> {
        self.fills.read().clone()
    }

    /// Get open order count.
    pub fn open_order_count(&self) -> usize {
        self.open_orders.read().len()
    }

    /// Total paper P&L (simplified: sum of buy notionals as negative, sell as positive).
    pub fn net_notional(&self) -> Decimal {
        self.fills
            .read()
            .iter()
            .map(|f| {
                if f.side == "Buy" {
                    -f.notional
                } else {
                    f.notional
                }
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::book::BookStore;
    use polymarket_client_sdk::clob::types::{OrderType, Side};
    use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, OrderBookLevel};
    use polymarket_client_sdk::types::{B256, U256};
    use rust_decimal_macros::dec;

    fn make_book(bid: Decimal, ask: Decimal) -> BookStore {
        let store = BookStore::new();
        let book = BookUpdate::builder()
            .asset_id(U256::ZERO)
            .market(B256::ZERO)
            .timestamp(0)
            .bids(vec![OrderBookLevel::builder()
                .price(bid)
                .size(dec!(100))
                .build()])
            .asks(vec![OrderBookLevel::builder()
                .price(ask)
                .size(dec!(100))
                .build()])
            .build();
        store.apply("token1", &book);
        store
    }

    fn intent(side: Side, price: Decimal, post_only: bool) -> OrderIntent {
        OrderIntent {
            token_id: "token1".into(),
            side,
            price,
            size: dec!(10),
            order_type: OrderType::GTC,
            post_only,
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        }
    }

    #[test]
    fn place_resting_order() {
        let engine = PaperEngine::new();
        let books = make_book(dec!(0.48), dec!(0.52));

        let (id, fill) = engine.place_order(&intent(Side::Buy, dec!(0.49), false), &books);
        assert!(id.starts_with("paper-"));
        assert!(fill.is_none());
        assert_eq!(engine.open_order_count(), 1);
        assert_eq!(engine.fills().len(), 0);
    }

    #[test]
    fn place_crossing_order_fills() {
        let engine = PaperEngine::new();
        let books = make_book(dec!(0.48), dec!(0.52));

        let (_id, fill) = engine.place_order(&intent(Side::Buy, dec!(0.52), false), &books);
        assert!(fill.is_some());
        assert_eq!(fill.unwrap().price, dec!(0.52));
        assert_eq!(engine.fills().len(), 1);
        assert_eq!(engine.open_order_count(), 0);
    }

    #[test]
    fn post_only_prevents_crossing_fill() {
        let engine = PaperEngine::new();
        let books = make_book(dec!(0.48), dec!(0.52));

        let (_id, fill) = engine.place_order(&intent(Side::Buy, dec!(0.52), true), &books);
        assert!(fill.is_none());
        assert_eq!(engine.fills().len(), 0);
        assert_eq!(engine.open_order_count(), 1);
    }

    #[test]
    fn cancel_order() {
        let engine = PaperEngine::new();
        let books = make_book(dec!(0.48), dec!(0.52));

        let (id, _) = engine.place_order(&intent(Side::Buy, dec!(0.49), false), &books);
        assert_eq!(engine.open_order_count(), 1);

        assert!(engine.cancel_order(&id));
        assert_eq!(engine.open_order_count(), 0);
        assert!(!engine.cancel_order(&id)); // already cancelled
    }

    #[test]
    fn cancel_all_orders() {
        let engine = PaperEngine::new();
        let books = make_book(dec!(0.48), dec!(0.52));

        engine.place_order(&intent(Side::Buy, dec!(0.47), false), &books);
        engine.place_order(&intent(Side::Buy, dec!(0.46), false), &books);
        engine.place_order(&intent(Side::Sell, dec!(0.55), false), &books);
        assert_eq!(engine.open_order_count(), 3);

        let count = engine.cancel_all();
        assert_eq!(count, 3);
        assert_eq!(engine.open_order_count(), 0);
    }

    #[test]
    fn sequential_ids() {
        let engine = PaperEngine::new();
        let books = make_book(dec!(0.48), dec!(0.52));

        let (id1, _) = engine.place_order(&intent(Side::Buy, dec!(0.47), false), &books);
        let (id2, _) = engine.place_order(&intent(Side::Buy, dec!(0.46), false), &books);
        assert_eq!(id1, "paper-1");
        assert_eq!(id2, "paper-2");
    }

    #[test]
    fn net_notional_tracking() {
        let engine = PaperEngine::new();
        let books = make_book(dec!(0.48), dec!(0.52));

        // Buy fills
        engine.place_order(&intent(Side::Buy, dec!(0.52), false), &books);
        // notional = 0.52 * 10 = 5.20, net = -5.20

        assert_eq!(engine.net_notional(), dec!(-5.20));
    }

    #[test]
    fn sell_crossing_fills() {
        let engine = PaperEngine::new();
        let books = make_book(dec!(0.48), dec!(0.52));

        let (_, fill) = engine.place_order(&intent(Side::Sell, dec!(0.48), false), &books);
        assert!(fill.is_some());
        assert_eq!(fill.unwrap().side, "Sell");
    }

    #[test]
    fn no_book_means_resting() {
        let engine = PaperEngine::new();
        let books = BookStore::new();

        let (_, fill) = engine.place_order(&intent(Side::Buy, dec!(0.52), false), &books);
        assert!(fill.is_none());
        assert_eq!(engine.open_order_count(), 1);
        assert_eq!(engine.fills().len(), 0);
    }
}
