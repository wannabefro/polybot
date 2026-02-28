use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::RwLock;
use polymarket_client_sdk::clob::ws::types::response::BookUpdate;
use rust_decimal::Decimal;
use tracing::debug;

/// A single price level.
#[derive(Debug, Clone)]
pub struct Level {
    pub price: Decimal,
    pub size: Decimal,
}

/// Snapshot of one side of the book for an asset.
#[derive(Debug, Clone, Default)]
pub struct BookSide {
    pub levels: Vec<Level>,
}

impl BookSide {
    pub fn best(&self) -> Option<&Level> {
        self.levels.first()
    }
}

/// Local orderbook for a single asset/token.
#[derive(Debug, Clone)]
pub struct LocalBook {
    pub asset_id: String,
    pub bids: BookSide,
    pub asks: BookSide,
    pub last_update: Instant,
    pub hash: Option<String>,
    pub timestamp_ms: i64,
}

impl LocalBook {
    pub fn mid_price(&self) -> Option<Decimal> {
        let best_bid = self.bids.best()?.price;
        let best_ask = self.asks.best()?.price;
        Some((best_bid + best_ask) / Decimal::TWO)
    }

    pub fn spread(&self) -> Option<Decimal> {
        let best_bid = self.bids.best()?.price;
        let best_ask = self.asks.best()?.price;
        Some(best_ask - best_bid)
    }

    pub fn is_stale(&self, threshold: std::time::Duration) -> bool {
        self.last_update.elapsed() > threshold
    }
}

/// Thread-safe orderbook store keyed by asset_id.
#[derive(Debug, Clone)]
pub struct BookStore {
    inner: Arc<RwLock<HashMap<String, LocalBook>>>,
}

impl BookStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Apply a BookUpdate from the WebSocket feed.
    pub fn apply(&self, asset_id: &str, update: &BookUpdate) {
        let bids = BookSide {
            levels: update
                .bids
                .iter()
                .map(|l| Level {
                    price: l.price,
                    size: l.size,
                })
                .collect(),
        };
        let asks = BookSide {
            levels: update
                .asks
                .iter()
                .map(|l| Level {
                    price: l.price,
                    size: l.size,
                })
                .collect(),
        };

        let book = LocalBook {
            asset_id: asset_id.to_string(),
            bids,
            asks,
            last_update: Instant::now(),
            hash: update.hash.clone(),
            timestamp_ms: update.timestamp,
        };

        debug!(asset_id, mid = ?book.mid_price(), "book: updated");
        self.inner.write().insert(asset_id.to_string(), book);
    }

    /// Get a snapshot of the book for an asset.
    pub fn get(&self, asset_id: &str) -> Option<LocalBook> {
        self.inner.read().get(asset_id).cloned()
    }

    /// Get mid prices for all tracked assets.
    pub fn all_mids(&self) -> HashMap<String, Decimal> {
        self.inner
            .read()
            .iter()
            .filter_map(|(id, book)| book.mid_price().map(|m| (id.clone(), m)))
            .collect()
    }

    /// Remove books for assets no longer in the universe.
    pub fn retain(&self, asset_ids: &[String]) {
        self.inner.write().retain(|k, _| asset_ids.contains(k));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, OrderBookLevel};
    use polymarket_client_sdk::types::{B256, U256};
    use rust_decimal_macros::dec;

    fn make_level(price: &str, size: &str) -> OrderBookLevel {
        OrderBookLevel::builder()
            .price(price.parse::<Decimal>().unwrap())
            .size(size.parse::<Decimal>().unwrap())
            .build()
    }

    fn make_update(bids: Vec<(&str, &str)>, asks: Vec<(&str, &str)>) -> BookUpdate {
        BookUpdate::builder()
            .asset_id(U256::ZERO)
            .market(B256::ZERO)
            .timestamp(1000)
            .bids(bids.into_iter().map(|(p, s)| make_level(p, s)).collect())
            .asks(asks.into_iter().map(|(p, s)| make_level(p, s)).collect())
            .hash("abc123".into())
            .build()
    }

    #[test]
    fn apply_and_get() {
        let store = BookStore::new();
        let update = make_update(vec![("0.48", "100")], vec![("0.52", "100")]);
        store.apply("token1", &update);

        let book = store.get("token1").unwrap();
        assert_eq!(book.asset_id, "token1");
        assert_eq!(book.bids.levels.len(), 1);
        assert_eq!(book.asks.levels.len(), 1);
        assert_eq!(book.hash, Some("abc123".into()));
    }

    #[test]
    fn mid_price_calculation() {
        let store = BookStore::new();
        let update = make_update(vec![("0.48", "100")], vec![("0.52", "100")]);
        store.apply("token1", &update);

        let book = store.get("token1").unwrap();
        let mid = book.mid_price().unwrap();
        assert_eq!(mid, dec!(0.5));
    }

    #[test]
    fn spread_calculation() {
        let store = BookStore::new();
        let update = make_update(vec![("0.48", "100")], vec![("0.52", "100")]);
        store.apply("token1", &update);

        let book = store.get("token1").unwrap();
        let spread = book.spread().unwrap();
        assert_eq!(spread, dec!(0.04));
    }

    #[test]
    fn empty_book_returns_none() {
        let store = BookStore::new();
        assert!(store.get("nonexistent").is_none());
    }

    #[test]
    fn empty_sides_no_mid() {
        let store = BookStore::new();
        let update = make_update(vec![], vec![]);
        store.apply("token1", &update);

        let book = store.get("token1").unwrap();
        assert!(book.mid_price().is_none());
        assert!(book.spread().is_none());
    }

    #[test]
    fn one_sided_book_no_mid() {
        let store = BookStore::new();
        let update = make_update(vec![("0.48", "100")], vec![]);
        store.apply("token1", &update);

        let book = store.get("token1").unwrap();
        assert!(book.mid_price().is_none());
    }

    #[test]
    fn stale_detection() {
        let store = BookStore::new();
        let update = make_update(vec![("0.48", "100")], vec![("0.52", "100")]);
        store.apply("token1", &update);

        let book = store.get("token1").unwrap();
        assert!(!book.is_stale(std::time::Duration::from_secs(5)));
    }

    #[test]
    fn update_replaces_previous() {
        let store = BookStore::new();
        store.apply("token1", &make_update(vec![("0.48", "100")], vec![("0.52", "100")]));
        store.apply("token1", &make_update(vec![("0.49", "200")], vec![("0.51", "200")]));

        let book = store.get("token1").unwrap();
        assert_eq!(book.bids.best().unwrap().price, dec!(0.49));
        assert_eq!(book.asks.best().unwrap().price, dec!(0.51));
        assert_eq!(book.mid_price().unwrap(), dec!(0.5));
        assert_eq!(book.spread().unwrap(), dec!(0.02));
    }

    #[test]
    fn all_mids_across_tokens() {
        let store = BookStore::new();
        store.apply("t1", &make_update(vec![("0.48", "100")], vec![("0.52", "100")]));
        store.apply("t2", &make_update(vec![("0.30", "50")], vec![("0.40", "50")]));
        store.apply("t3", &make_update(vec![], vec![]));

        let mids = store.all_mids();
        assert_eq!(mids.len(), 2);
        assert_eq!(mids["t1"], dec!(0.5));
        assert_eq!(mids["t2"], dec!(0.35));
    }

    #[test]
    fn retain_removes_stale_assets() {
        let store = BookStore::new();
        store.apply("t1", &make_update(vec![("0.48", "100")], vec![("0.52", "100")]));
        store.apply("t2", &make_update(vec![("0.30", "50")], vec![("0.40", "50")]));
        store.apply("t3", &make_update(vec![("0.60", "50")], vec![("0.70", "50")]));

        store.retain(&["t1".into(), "t3".into()]);
        assert!(store.get("t1").is_some());
        assert!(store.get("t2").is_none());
        assert!(store.get("t3").is_some());
    }

    #[test]
    fn multiple_levels() {
        let store = BookStore::new();
        let update = make_update(
            vec![("0.48", "100"), ("0.47", "200"), ("0.46", "300")],
            vec![("0.52", "100"), ("0.53", "200")],
        );
        store.apply("token1", &update);

        let book = store.get("token1").unwrap();
        assert_eq!(book.bids.levels.len(), 3);
        assert_eq!(book.asks.levels.len(), 2);
        assert_eq!(book.bids.best().unwrap().price, dec!(0.48));
        assert_eq!(book.asks.best().unwrap().price, dec!(0.52));
    }
}
