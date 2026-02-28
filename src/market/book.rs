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
