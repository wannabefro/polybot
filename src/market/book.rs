use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::RwLock;
use polymarket_client_sdk::clob::ws::types::response::BookUpdate;
use rust_decimal::Decimal;
use tracing::{debug, warn};

/// A single price level.
#[derive(Debug, Clone)]
pub struct Level {
    pub price: Decimal,
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub asset_id: String,
    pub bids: BookSide,
    pub asks: BookSide,
    pub last_update: Instant,
    #[allow(dead_code)]
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
    /// Pause flag: set on WS reconnect, cleared after successful resync.
    paused: Arc<AtomicBool>,
}

impl BookStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            paused: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Apply a BookUpdate from the WebSocket feed.
    /// Rejects timestamp regressions (stale/replayed data).
    /// Returns false if the update was rejected.
    pub fn apply(&self, asset_id: &str, update: &BookUpdate) -> bool {
        // Check timestamp regression
        if let Some(existing) = self.inner.read().get(asset_id) {
            if update.timestamp < existing.timestamp_ms {
                warn!(
                    asset_id,
                    new_ts = update.timestamp,
                    old_ts = existing.timestamp_ms,
                    "book: rejected timestamp regression"
                );
                return false;
            }
        }

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
        true
    }

    /// Get a snapshot of the book for an asset.
    pub fn get(&self, asset_id: &str) -> Option<LocalBook> {
        self.inner.read().get(asset_id).cloned()
    }

    /// Get mid prices for all tracked assets.
    #[allow(dead_code)]
    pub fn all_mids(&self) -> HashMap<String, Decimal> {
        self.inner
            .read()
            .iter()
            .filter_map(|(id, book)| book.mid_price().map(|m| (id.clone(), m)))
            .collect()
    }

    /// Remove books for assets no longer in the universe.
    #[allow(dead_code)]
    pub fn retain(&self, asset_ids: &[String]) {
        self.inner.write().retain(|k, _| asset_ids.contains(k));
    }

    /// Pause quoting (set on WS reconnect).
    #[allow(dead_code)]
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Relaxed);
        warn!("book: paused — awaiting resync");
    }

    /// Resume quoting after successful resync.
    pub fn resume(&self) {
        self.paused.store(false, Ordering::Relaxed);
    }

    /// Check if quoting is paused (e.g., during WS reconnect resync).
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Check if any tracked book has a stale feed.
    #[allow(dead_code)]
    pub fn any_stale(&self, threshold: std::time::Duration) -> bool {
        self.inner.read().values().any(|b| b.is_stale(threshold))
    }

    /// Return the stored hash from the last book update for a given token.
    #[allow(dead_code)]
    pub fn hash(&self, token_id: &str) -> Option<String> {
        self.inner.read().get(token_id).and_then(|b| b.hash.clone())
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

    // ── New: timestamp regression tests ──

    fn make_update_ts(
        bids: Vec<(&str, &str)>,
        asks: Vec<(&str, &str)>,
        ts: i64,
    ) -> BookUpdate {
        BookUpdate::builder()
            .asset_id(U256::ZERO)
            .market(B256::ZERO)
            .timestamp(ts)
            .bids(bids.into_iter().map(|(p, s)| make_level(p, s)).collect())
            .asks(asks.into_iter().map(|(p, s)| make_level(p, s)).collect())
            .hash("abc123".into())
            .build()
    }

    #[test]
    fn timestamp_regression_rejected() {
        let store = BookStore::new();
        let u1 = make_update_ts(vec![("0.48", "100")], vec![("0.52", "100")], 2000);
        assert!(store.apply("t1", &u1));

        // Older timestamp should be rejected
        let u2 = make_update_ts(vec![("0.49", "50")], vec![("0.51", "50")], 1000);
        assert!(!store.apply("t1", &u2));

        // Book should still have the original data
        let book = store.get("t1").unwrap();
        assert_eq!(book.bids.best().unwrap().price, dec!(0.48));
        assert_eq!(book.timestamp_ms, 2000);
    }

    #[test]
    fn same_timestamp_accepted() {
        let store = BookStore::new();
        let u1 = make_update_ts(vec![("0.48", "100")], vec![("0.52", "100")], 1000);
        assert!(store.apply("t1", &u1));

        let u2 = make_update_ts(vec![("0.49", "100")], vec![("0.51", "100")], 1000);
        assert!(store.apply("t1", &u2));
    }

    #[test]
    fn newer_timestamp_accepted() {
        let store = BookStore::new();
        let u1 = make_update_ts(vec![("0.48", "100")], vec![("0.52", "100")], 1000);
        assert!(store.apply("t1", &u1));

        let u2 = make_update_ts(vec![("0.49", "100")], vec![("0.51", "100")], 2000);
        assert!(store.apply("t1", &u2));

        let book = store.get("t1").unwrap();
        assert_eq!(book.bids.best().unwrap().price, dec!(0.49));
    }

    // ── New: pause/resume tests ──

    #[test]
    fn pause_and_resume() {
        let store = BookStore::new();
        assert!(!store.is_paused());
        store.pause();
        assert!(store.is_paused());
        store.resume();
        assert!(!store.is_paused());
    }

    #[test]
    fn pause_does_not_block_apply() {
        let store = BookStore::new();
        store.pause();
        let u = make_update(vec![("0.48", "100")], vec![("0.52", "100")]);
        assert!(store.apply("t1", &u));
        assert!(store.get("t1").is_some());
    }

    #[test]
    fn any_stale_detection() {
        let store = BookStore::new();
        let u = make_update(vec![("0.48", "100")], vec![("0.52", "100")]);
        store.apply("t1", &u);
        // Just applied — not stale
        assert!(!store.any_stale(std::time::Duration::from_secs(5)));
    }

    #[test]
    fn hash_returns_stored_value() {
        let store = BookStore::new();
        let u = make_update(vec![("0.48", "100")], vec![("0.52", "100")]);
        store.apply("t1", &u);
        assert_eq!(store.hash("t1"), Some("abc123".into()));
    }

    #[test]
    fn hash_returns_none_for_missing() {
        let store = BookStore::new();
        assert!(store.hash("nonexistent").is_none());
    }
}
