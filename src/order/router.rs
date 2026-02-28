//! Order routing: dispatches to paper engine or live CLOB pipeline.

use std::sync::Arc;

use anyhow::Result;
use rust_decimal::Decimal;
use tracing::info;

use crate::auth::AuthContext;
use crate::config::Config;
use crate::market::book::BookStore;
use crate::ops::paper::{PaperEngine, PaperFill};
use crate::order::pipeline::{self, OrderIntent};

/// Result of placing an order through the router.
#[derive(Debug)]
#[allow(dead_code)]
pub struct PlaceResult {
    pub order_id: String,
    /// If the paper engine filled immediately, this is Some.
    pub paper_fill: Option<PaperFill>,
    /// In live mode, if the CLOB matched (e.g. FOK order), this is true.
    pub live_matched: bool,
    /// In live mode, the intent that was submitted (for recording fills).
    pub live_intent: Option<OrderIntent>,
    /// Actual fill size from the CLOB (making_amount). Use instead of intent.size.
    pub fill_size: Decimal,
    /// Actual fill notional from the CLOB (taking_amount). Use instead of intent.price * intent.size.
    pub fill_notional: Decimal,
}

/// Unified order routing — paper or live.
pub enum OrderRouter {
    Paper(Arc<PaperEngine>),
    Live(AuthContext),
}

impl OrderRouter {
    pub fn new(config: &Config, auth: AuthContext) -> Self {
        if config.paper_mode {
            info!("router: paper mode — no real orders will be placed");
            Self::Paper(PaperEngine::new())
        } else {
            info!("router: LIVE mode — orders will hit the CLOB");
            Self::Live(auth)
        }
    }

    /// Place an order through the appropriate backend.
    /// Returns the order ID and (in paper mode) any immediate fill.
    pub async fn place(&self, intent: &OrderIntent, books: &BookStore) -> Result<PlaceResult> {
        match self {
            Self::Paper(engine) => {
                let (id, paper_fill) = engine.place_order(intent, books);
                Ok(PlaceResult {
                    order_id: id,
                    paper_fill,
                    live_matched: false,
                    live_intent: None,
                    fill_size: Decimal::ZERO,
                    fill_notional: Decimal::ZERO,
                })
            }
            Self::Live(ctx) => {
                let result = pipeline::place_maker_order(ctx, intent).await?;
                Ok(PlaceResult {
                    order_id: result.order_id,
                    paper_fill: None,
                    live_matched: result.matched,
                    live_intent: Some(intent.clone()),
                    fill_size: result.making_amount,
                    fill_notional: result.taking_amount,
                })
            }
        }
    }

    /// Cancel a specific order.
    #[allow(dead_code)]
    pub async fn cancel(&self, order_id: &str) -> Result<()> {
        match self {
            Self::Paper(engine) => {
                engine.cancel_order(order_id);
                Ok(())
            }
            Self::Live(ctx) => pipeline::cancel(&ctx.client, order_id).await,
        }
    }

    /// Emergency cancel-all.
    pub async fn cancel_all(&self) -> Result<()> {
        match self {
            Self::Paper(engine) => {
                let n = engine.cancel_all();
                info!(cancelled = n, "router: paper cancel-all");
                Ok(())
            }
            Self::Live(ctx) => pipeline::cancel_all(&ctx.client).await,
        }
    }

    /// Get the paper engine reference (for metrics/inspection in paper mode).
    #[allow(dead_code)]
    pub fn paper_engine(&self) -> Option<&Arc<PaperEngine>> {
        match self {
            Self::Paper(engine) => Some(engine),
            Self::Live(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::tests::test_config;
    use crate::market::book::BookStore;
    use crate::order::pipeline::OrderIntent;
    use polymarket_client_sdk::clob::types::{OrderType, Side};
    use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, OrderBookLevel};
    use polymarket_client_sdk::types::{B256, U256};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn make_book_store() -> BookStore {
        let store = BookStore::new();
        let update = BookUpdate::builder()
            .asset_id(U256::ZERO)
            .market(B256::ZERO)
            .timestamp(0)
            .bids(vec![OrderBookLevel::builder()
                .price("0.48".parse::<Decimal>().unwrap())
                .size(dec!(100))
                .build()])
            .asks(vec![OrderBookLevel::builder()
                .price("0.52".parse::<Decimal>().unwrap())
                .size(dec!(100))
                .build()])
            .build();
        store.apply("token1", &update);
        store
    }

    fn test_intent() -> OrderIntent {
        OrderIntent {
            token_id: "token1".into(),
            side: Side::Buy,
            price: dec!(0.49),
            size: dec!(10),
            order_type: OrderType::GTC,
            post_only: true,
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        }
    }

    #[tokio::test]
    async fn paper_router_places_order() {
        let engine = PaperEngine::new();
        let router = OrderRouter::Paper(engine);
        let books = make_book_store();

        let result = router.place(&test_intent(), &books).await.unwrap();
        assert!(result.order_id.starts_with("paper-"));
    }

    #[tokio::test]
    async fn paper_router_cancel_all() {
        let engine = PaperEngine::new();
        let router = OrderRouter::Paper(engine);
        let books = make_book_store();

        router.place(&test_intent(), &books).await.unwrap();
        router.cancel_all().await.unwrap();
        assert_eq!(router.paper_engine().unwrap().open_order_count(), 0);
    }

    #[tokio::test]
    async fn paper_router_cancel_specific() {
        let engine = PaperEngine::new();
        let router = OrderRouter::Paper(engine);
        let books = make_book_store();

        let result = router.place(&test_intent(), &books).await.unwrap();
        router.cancel(&result.order_id).await.unwrap();
        assert_eq!(router.paper_engine().unwrap().open_order_count(), 0);
    }

    #[tokio::test]
    async fn paper_router_detects_immediate_fill() {
        let engine = PaperEngine::new();
        let router = OrderRouter::Paper(engine);
        let books = make_book_store();

        // Buy at 0.52 crosses the ask at 0.52 → immediate fill
        let crossing_intent = OrderIntent {
            token_id: "token1".into(),
            side: Side::Buy,
            price: dec!(0.52),
            size: dec!(10),
            order_type: OrderType::GTC,
            post_only: false,
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        };
        let result = router.place(&crossing_intent, &books).await.unwrap();
        assert!(result.paper_fill.is_some(), "crossing order should fill immediately");
        let fill = result.paper_fill.unwrap();
        assert_eq!(fill.size, dec!(10));
    }

    #[tokio::test]
    async fn paper_router_no_fill_for_resting() {
        let engine = PaperEngine::new();
        let router = OrderRouter::Paper(engine);
        let books = make_book_store();

        // Buy at 0.49 doesn't cross the ask at 0.52 → rests
        let result = router.place(&test_intent(), &books).await.unwrap();
        assert!(result.paper_fill.is_none(), "resting order should not fill");
    }

    #[test]
    fn paper_engine_accessor() {
        let engine = PaperEngine::new();
        let router = OrderRouter::Paper(engine);
        assert!(router.paper_engine().is_some());
    }
}
