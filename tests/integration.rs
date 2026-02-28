//! Integration tests: full paper-mode pipeline tick.
//!
//! These tests wire real modules together (BookStore, RiskEngine, strategies,
//! PaperEngine, Metrics) and run quoting cycles without any network calls.

use polybot::config::test_config;
use polybot::market::book::BookStore;
use polybot::market::discovery::{TradableMarket, TokenInfo};
use polybot::ops::metrics::Metrics;
use polybot::ops::paper::PaperEngine;
use polybot::order::pipeline::OrderIntent;
use polybot::order::router::OrderRouter;
use polybot::risk::guardrails::{RiskEngine, RiskVerdict};
use polybot::strategy;

use polymarket_client_sdk::clob::types::{OrderType, Side};
use polymarket_client_sdk::clob::ws::types::response::{BookUpdate, OrderBookLevel};
use polymarket_client_sdk::types::{B256, U256};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ─── Helpers ──────────────────────────────────────────────────

fn make_market(rewards: bool, volume: f64) -> TradableMarket {
    TradableMarket {
        condition_id: "cond1".into(),
        question: "Will ETH hit $5k?".into(),
        tokens: vec![
            TokenInfo {
                token_id: "token_yes".into(),
                outcome: "Yes".into(),
                price: dec!(0.50),
            },
            TokenInfo {
                token_id: "token_no".into(),
                outcome: "No".into(),
                price: dec!(0.50),
            },
        ],
        neg_risk: false,
        neg_risk_market_id: None,
        min_tick_size: dec!(0.01),
        min_order_size: dec!(5),
        maker_fee_bps: dec!(0),
        rewards_active: rewards,
        volume_24h: volume,
        tags: vec![],
    }
}

fn seed_book(store: &BookStore, token_id: &str, bid: &str, ask: &str) {
    let update = BookUpdate::builder()
        .asset_id(U256::ZERO)
        .market(B256::ZERO)
        .timestamp(0)
        .bids(vec![OrderBookLevel::builder()
            .price(bid.parse::<Decimal>().unwrap())
            .size(dec!(500))
            .build()])
        .asks(vec![OrderBookLevel::builder()
            .price(ask.parse::<Decimal>().unwrap())
            .size(dec!(500))
            .build()])
        .build();
    store.apply(token_id, &update);
}

// ─── Tests ────────────────────────────────────────────────────

/// Full pipeline: discovery → book → rebate_mm → paper engine → metrics.
#[tokio::test]
async fn paper_pipeline_rebate_mm_tick() {
    let cfg = test_config();
    let books = BookStore::new();
    let risk = RiskEngine::new(cfg.clone());
    let paper = PaperEngine::new();
    let router = OrderRouter::Paper(paper);
    let metrics = Metrics::new();

    let market = make_market(false, 50_000.0);
    seed_book(&books, "token_yes", "0.48", "0.52");

    // Generate quotes
    let quotes = strategy::rebate_mm::generate_quotes(&cfg, &market, &books, &risk);
    assert!(quotes.is_some(), "should generate quotes for a valid market");

    let (bid, ask) = quotes.unwrap();
    assert!(matches!(bid.side, Side::Buy));
    assert!(matches!(ask.side, Side::Sell));
    assert!(bid.post_only);
    assert!(ask.post_only);

    // Route through paper engine
    let bid_id = router.place(&bid, &books).await.unwrap();
    metrics.inc_quotes_sent();
    let ask_id = router.place(&ask, &books).await.unwrap();
    metrics.inc_quotes_sent();

    assert!(bid_id.starts_with("paper-"));
    assert!(ask_id.starts_with("paper-"));

    let snap = metrics.snapshot();
    assert_eq!(snap.quotes_sent, 2);
}

/// Full pipeline: reward strategy generates quotes and routes them.
#[tokio::test]
async fn paper_pipeline_reward_tick() {
    let cfg = test_config();
    let books = BookStore::new();
    let risk = RiskEngine::new(cfg.clone());
    let paper = PaperEngine::new();
    let router = OrderRouter::Paper(paper);

    let market = make_market(true, 50_000.0);
    seed_book(&books, "token_yes", "0.48", "0.52");

    let quotes = strategy::reward::evaluate_reward_quote(&cfg, &market, &books, &risk);
    assert!(quotes.is_some(), "reward market should produce quotes");

    let (bid, ask) = quotes.unwrap();
    router.place(&bid, &books).await.unwrap();
    router.place(&ask, &books).await.unwrap();

    assert_eq!(router.paper_engine().unwrap().open_order_count(), 2);
}

/// Risk engine rejects orders after daily loss exceeds 3% NAV.
#[tokio::test]
async fn risk_daily_loss_rejects_orders() {
    let cfg = test_config();
    let risk = RiskEngine::new(cfg.clone());

    // Simulate 3.1% NAV loss (NAV=10000, loss=310)
    risk.record_pnl(dec!(-310));

    // Orders should be rejected by the daily-loss check
    let intent = OrderIntent {
        token_id: "token_yes".into(),
        side: Side::Buy,
        price: dec!(0.49),
        size: dec!(10),
        order_type: OrderType::GTC,
        post_only: true,
    };
    let verdict = risk.check("cond1", &intent);
    assert!(
        matches!(verdict, RiskVerdict::Rejected(_)),
        "orders should be rejected when daily loss exceeded"
    );
}

/// Explicit halt stops all order checks.
#[tokio::test]
async fn risk_halt_stops_all_orders() {
    let cfg = test_config();
    let risk = RiskEngine::new(cfg.clone());

    risk.halt("test halt");
    assert!(risk.is_halted());

    let intent = OrderIntent {
        token_id: "token_yes".into(),
        side: Side::Buy,
        price: dec!(0.49),
        size: dec!(10),
        order_type: OrderType::GTC,
        post_only: true,
    };
    let verdict = risk.check("cond1", &intent);
    assert!(matches!(verdict, RiskVerdict::Rejected(_)));
}

/// Daily reset clears P&L and allows trading again.
#[tokio::test]
async fn risk_daily_reset_clears_loss() {
    let cfg = test_config();
    let risk = RiskEngine::new(cfg.clone());

    risk.record_pnl(dec!(-310));

    // Should reject
    let intent = OrderIntent {
        token_id: "token_yes".into(),
        side: Side::Buy,
        price: dec!(0.49),
        size: dec!(10),
        order_type: OrderType::GTC,
        post_only: true,
    };
    assert!(matches!(risk.check("cond1", &intent), RiskVerdict::Rejected(_)));

    risk.reset_daily();

    // Should approve now
    assert!(matches!(risk.check("cond1", &intent), RiskVerdict::Approved));
}

/// Hedge SLA breach triggers emergency state.
#[tokio::test]
async fn hedge_sla_breach_triggers_emergency() {
    let mut tracker = strategy::reward::HedgeTracker::new(Duration::from_millis(500));

    // Fill that's already expired (1 second old, SLA is 500ms)
    tracker.record_fill(strategy::reward::UnhedgedFill {
        token_id: "token_yes".into(),
        side: Side::Buy,
        price: dec!(0.52),
        size: dec!(10),
        filled_at: Instant::now() - Duration::from_secs(1),
    });

    assert!(tracker.has_emergency(), "should be in emergency state");
    assert_eq!(tracker.breached_fills().len(), 1);

    // Emergency response: cancel-all would be called, then clear tracker
    tracker.clear();
    assert!(!tracker.has_emergency());
}

/// Hedge tracker generates correct hedge orders.
#[tokio::test]
async fn hedge_tracker_generates_opposing_orders() {
    let books = BookStore::new();
    seed_book(&books, "token_yes", "0.48", "0.52");

    let mut tracker = strategy::reward::HedgeTracker::new(Duration::from_millis(500));
    tracker.record_fill(strategy::reward::UnhedgedFill {
        token_id: "token_yes".into(),
        side: Side::Buy,
        price: dec!(0.52),
        size: dec!(10),
        filled_at: Instant::now(),
    });

    let hedges = tracker.pending_hedges(&books);
    assert_eq!(hedges.len(), 1);
    assert!(matches!(hedges[0].side, Side::Sell), "hedge for buy should sell");
    assert_eq!(hedges[0].price, dec!(0.48), "should sell at best bid");
    assert!(!hedges[0].post_only, "hedges should not be post-only");
}

/// Mean reversion: full cycle from price recording to exit.
#[tokio::test]
async fn mean_revert_full_cycle() {
    let cfg = test_config();
    let books = BookStore::new();
    let risk = RiskEngine::new(cfg.clone());
    let paper = PaperEngine::new();
    let router = OrderRouter::Paper(paper);

    let market = make_market(false, 50_000.0);

    // Build SMA around 0.50
    let mut state = strategy::mean_revert::MeanRevertState::new();
    for _ in 0..15 {
        state.record_price("token_yes", dec!(0.50));
    }

    // Price shoots up to 0.60 — should trigger sell
    seed_book(&books, "token_yes", "0.59", "0.61");

    let intent = strategy::mean_revert::evaluate_entry(&cfg, &market, &books, &risk, &state);
    assert!(intent.is_some(), "should signal entry on deviation");
    let intent = intent.unwrap();
    assert!(matches!(intent.side, Side::Sell), "should sell when above SMA");

    // Place the order
    router.place(&intent, &books).await.unwrap();

    // Track position
    state.open_position(strategy::mean_revert::MeanRevertPosition {
        token_id: intent.token_id.clone(),
        side: intent.side,
        entry_price: intent.price,
        size: intent.size,
        opened_at: Instant::now(),
    });

    assert_eq!(state.open_positions().len(), 1);
}

/// Multiple strategies can coexist on the same market without interfering.
#[tokio::test]
async fn multi_strategy_coexistence() {
    let cfg = test_config();
    let books = BookStore::new();
    let risk = RiskEngine::new(cfg.clone());
    let paper = PaperEngine::new();
    let router = OrderRouter::Paper(paper);
    let metrics = Metrics::new();

    let market = make_market(true, 50_000.0);
    seed_book(&books, "token_yes", "0.48", "0.52");

    // Rebate MM generates quotes
    if let Some((bid, ask)) = strategy::rebate_mm::generate_quotes(&cfg, &market, &books, &risk) {
        router.place(&bid, &books).await.unwrap();
        router.place(&ask, &books).await.unwrap();
        metrics.inc_quotes_sent();
        metrics.inc_quotes_sent();
    }

    // Reward also generates quotes (same market, different quotes)
    if let Some((bid, ask)) =
        strategy::reward::evaluate_reward_quote(&cfg, &market, &books, &risk)
    {
        router.place(&bid, &books).await.unwrap();
        router.place(&ask, &books).await.unwrap();
        metrics.inc_quotes_sent();
        metrics.inc_quotes_sent();
    }

    let engine = router.paper_engine().unwrap();
    assert!(
        engine.open_order_count() >= 2,
        "should have orders from at least one strategy"
    );
    assert!(metrics.snapshot().quotes_sent >= 2);
}

/// Cancel-all through router clears all paper orders.
#[tokio::test]
async fn cancel_all_clears_paper_engine() {
    let cfg = test_config();
    let books = BookStore::new();
    let risk = RiskEngine::new(cfg.clone());
    let paper = PaperEngine::new();
    let router = OrderRouter::Paper(paper);

    let market = make_market(true, 50_000.0);
    seed_book(&books, "token_yes", "0.48", "0.52");

    if let Some((bid, ask)) = strategy::rebate_mm::generate_quotes(&cfg, &market, &books, &risk) {
        router.place(&bid, &books).await.unwrap();
        router.place(&ask, &books).await.unwrap();
    }

    assert!(router.paper_engine().unwrap().open_order_count() > 0);

    router.cancel_all().await.unwrap();
    assert_eq!(router.paper_engine().unwrap().open_order_count(), 0);
}
