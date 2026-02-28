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
        rewards_max_spread: None,
        rewards_min_size: None,
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
    assert!(!quotes.is_empty(), "should generate quotes for a valid market");

    let (bid, ask) = &quotes[0];
    assert!(matches!(bid.side, Side::Buy));
    assert!(matches!(ask.side, Side::Sell));
    assert!(bid.post_only);
    assert!(ask.post_only);

    // Route through paper engine
    let bid_result = router.place(bid, &books).await.unwrap();
    metrics.inc_quotes_sent();
    let ask_result = router.place(ask, &books).await.unwrap();
    metrics.inc_quotes_sent();

    assert!(bid_result.order_id.starts_with("paper-"));
    assert!(ask_result.order_id.starts_with("paper-"));

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
    assert!(!quotes.is_empty(), "reward market should produce quotes");

    let (bid, ask) = &quotes[0];
    router.place(bid, &books).await.unwrap();
    router.place(ask, &books).await.unwrap();

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
    neg_risk: false,
    fee_rate_bps: Decimal::ZERO,
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
    neg_risk: false,
    fee_rate_bps: Decimal::ZERO,
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
    neg_risk: false,
    fee_rate_bps: Decimal::ZERO,
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
        neg_risk: false,
        fee_rate_bps: Decimal::ZERO,
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
        neg_risk: false,
        fee_rate_bps: Decimal::ZERO,
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
    for (bid, ask) in strategy::rebate_mm::generate_quotes(&cfg, &market, &books, &risk) {
        router.place(&bid, &books).await.unwrap();
        router.place(&ask, &books).await.unwrap();
        metrics.inc_quotes_sent();
        metrics.inc_quotes_sent();
    }

    // Reward also generates quotes (same market, different quotes)
    for (bid, ask) in
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

    for (bid, ask) in strategy::rebate_mm::generate_quotes(&cfg, &market, &books, &risk) {
        router.place(&bid, &books).await.unwrap();
        router.place(&ask, &books).await.unwrap();
    }

    assert!(router.paper_engine().unwrap().open_order_count() > 0);

    router.cancel_all().await.unwrap();
    assert_eq!(router.paper_engine().unwrap().open_order_count(), 0);
}

// ─── Phase 3 Gap Tests ───────────────────────────────────────

/// Gap #1: Fill detection — paper engine returns fills through router.
#[tokio::test]
async fn fill_detection_through_router() {
    let paper = PaperEngine::new();
    let router = OrderRouter::Paper(paper);
    let books = BookStore::new();
    seed_book(&books, "token_yes", "0.48", "0.52");

    // Crossing buy at 0.52 should fill immediately
    let intent = OrderIntent {
        token_id: "token_yes".into(),
        side: Side::Buy,
        price: dec!(0.52),
        size: dec!(10),
        order_type: OrderType::GTC,
        post_only: false,
    neg_risk: false,
    fee_rate_bps: Decimal::ZERO,
    };
    let result = router.place(&intent, &books).await.unwrap();
    assert!(result.paper_fill.is_some());
    let fill = result.paper_fill.unwrap();
    assert_eq!(fill.token_id, "token_yes");
    assert_eq!(fill.size, dec!(10));
}

/// Gap #2: Cancel failure threshold halts risk engine.
#[tokio::test]
async fn cancel_failure_threshold_halts_engine() {
    let cfg = test_config();
    let risk = RiskEngine::new(cfg.clone());

    // 3 consecutive failures should halt
    assert!(!risk.record_cancel_failure());
    assert!(!risk.record_cancel_failure());
    assert!(risk.record_cancel_failure()); // 3rd triggers halt
    assert!(risk.is_halted());

    // All orders now rejected
    let intent = OrderIntent {
        token_id: "token_yes".into(),
        side: Side::Buy,
        price: dec!(0.49),
        size: dec!(10),
        order_type: OrderType::GTC,
        post_only: true,
    neg_risk: false,
    fee_rate_bps: Decimal::ZERO,
    };
    assert!(matches!(risk.check("cond1", &intent), RiskVerdict::Rejected(_)));
}

/// Gap #3: One-sided inventory blocks excessive directional exposure.
#[tokio::test]
async fn one_sided_inventory_blocks_excessive_exposure() {
    let cfg = test_config();
    let risk = RiskEngine::new(cfg.clone());

    // max_one_sided_inventory = 0.01 → 1% of 10000 = 100 USDC
    // Record buy 200 shares at some implied price
    risk.record_fill("cond1", "token_yes", Side::Buy, Decimal::from(200), Decimal::from(100));

    // Further buying should be blocked (200 shares * 0.50 = 100 notional = at limit)
    let intent = OrderIntent {
        token_id: "token_yes".into(),
        side: Side::Buy,
        price: dec!(0.50),
        size: dec!(10),
        order_type: OrderType::GTC,
        post_only: true,
    neg_risk: false,
    fee_rate_bps: Decimal::ZERO,
    };
    assert!(matches!(risk.check("cond1", &intent), RiskVerdict::Rejected(_)));

    // But selling should be fine (reduces inventory)
    let sell_intent = OrderIntent {
        token_id: "token_yes".into(),
        side: Side::Sell,
        price: dec!(0.50),
        size: dec!(50),
        order_type: OrderType::GTC,
        post_only: true,
    neg_risk: false,
    fee_rate_bps: Decimal::ZERO,
    };
    assert!(matches!(risk.check("cond1", &sell_intent), RiskVerdict::Approved));
}

/// Gap #5: Book timestamp regression is rejected.
#[tokio::test]
async fn book_timestamp_regression_rejected() {
    let books = BookStore::new();

    let u1 = BookUpdate::builder()
        .asset_id(U256::ZERO)
        .market(B256::ZERO)
        .timestamp(2000)
        .bids(vec![OrderBookLevel::builder()
            .price(dec!(0.48))
            .size(dec!(100))
            .build()])
        .asks(vec![OrderBookLevel::builder()
            .price(dec!(0.52))
            .size(dec!(100))
            .build()])
        .build();
    assert!(books.apply("token_yes", &u1));

    // Older update should be rejected
    let u2 = BookUpdate::builder()
        .asset_id(U256::ZERO)
        .market(B256::ZERO)
        .timestamp(1000)
        .bids(vec![OrderBookLevel::builder()
            .price(dec!(0.40))
            .size(dec!(50))
            .build()])
        .asks(vec![OrderBookLevel::builder()
            .price(dec!(0.60))
            .size(dec!(50))
            .build()])
        .build();
    assert!(!books.apply("token_yes", &u2));

    // Book should still have original data
    let book = books.get("token_yes").unwrap();
    assert_eq!(book.bids.best().unwrap().price, dec!(0.48));
}

/// Gap #5: Book pause/resume cycle.
#[tokio::test]
async fn book_pause_resume_cycle() {
    let books = BookStore::new();

    assert!(!books.is_paused());
    books.pause();
    assert!(books.is_paused());
    books.resume();
    assert!(!books.is_paused());
}

/// Gap #6: Neg-risk pair freshness (unit-level check).
#[tokio::test]
async fn neg_risk_stale_pair_detection() {
    let books = BookStore::new();

    // Seed one token but not the other in a neg-risk pair
    seed_book(&books, "token_yes", "0.48", "0.52");
    // token_no has no book → stale

    let stale = books.get("token_no").is_none();
    assert!(stale, "missing book should be detected as stale");
}

// ─── Phase 4 Tests ────────────────────────────────────────────

/// Phase 4 Bug #2: Paper fill race — place_order returns fill atomically.
#[tokio::test]
async fn paper_fill_returned_atomically() {
    let paper = PaperEngine::new();
    let books = BookStore::new();
    seed_book(&books, "token_yes", "0.48", "0.52");

    // Crossing buy should return fill directly from place_order
    let (id, fill) = paper.place_order(
        &OrderIntent {
            token_id: "token_yes".into(),
            side: Side::Buy,
            price: dec!(0.52),
            size: dec!(10),
            order_type: OrderType::GTC,
            post_only: false,
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        },
        &books,
    );
    assert!(id.starts_with("paper-"));
    assert!(fill.is_some(), "crossing order should return fill atomically");
    let f = fill.unwrap();
    assert_eq!(f.token_id, "token_yes");
    assert_eq!(f.size, dec!(10));
}

/// Phase 4 Bug #2: Resting order returns None fill.
#[tokio::test]
async fn paper_resting_returns_no_fill() {
    let paper = PaperEngine::new();
    let books = BookStore::new();
    seed_book(&books, "token_yes", "0.48", "0.52");

    let (_, fill) = paper.place_order(
        &OrderIntent {
            token_id: "token_yes".into(),
            side: Side::Buy,
            price: dec!(0.49),
            size: dec!(10),
            order_type: OrderType::GTC,
            post_only: true,
            neg_risk: false,
            fee_rate_bps: Decimal::ZERO,
        },
        &books,
    );
    assert!(fill.is_none(), "resting order should not return a fill");
}

/// Phase 4 Bug #3: Inventory snapshot is non-empty after fills.
#[tokio::test]
async fn inventory_snapshot_for_recon() {
    let cfg = test_config();
    let risk = RiskEngine::new(cfg.clone());

    risk.record_fill("cond1", "token_yes", Side::Buy, Decimal::from(100), Decimal::from(50));
    risk.record_fill("cond1", "token_no", Side::Sell, Decimal::from(50), Decimal::from(25));

    let snapshot = risk.inventory_snapshot();
    assert_eq!(snapshot.len(), 2);
    assert_eq!(*snapshot.get("token_yes").unwrap(), Decimal::from(100));
    assert_eq!(*snapshot.get("token_no").unwrap(), Decimal::from(-50));
}

/// Phase 4: OrderIntent carries neg_risk and fee_rate_bps through pipeline.
#[tokio::test]
async fn order_intent_carries_neg_risk_fields() {
    let intent = OrderIntent {
        token_id: "token_yes".into(),
        side: Side::Buy,
        price: dec!(0.50),
        size: dec!(10),
        order_type: OrderType::GTC,
        post_only: true,
        neg_risk: true,
        fee_rate_bps: dec!(200),
    };
    assert!(intent.neg_risk);
    assert_eq!(intent.fee_rate_bps, dec!(200));
}

/// Phase 4: Inventory skew adjusts quotes away from concentrated side.
#[tokio::test]
async fn inventory_skew_shifts_quotes() {
    let cfg = test_config();
    let risk = RiskEngine::new(cfg.clone());
    let books = BookStore::new();
    seed_book(&books, "token_yes", "0.48", "0.52");

    let market = make_market(true, 50_000.0);

    // Without inventory, get baseline quotes
    let quotes_neutral = strategy::rebate_mm::generate_quotes(&cfg, &market, &books, &risk);
    assert!(!quotes_neutral.is_empty());
    let (bid_n, _ask_n) = &quotes_neutral[0];

    // Record moderate long position (50 shares at 0.50 = 25 notional, well within 100 limit)
    risk.record_fill("cond1", "token_yes", Side::Buy, Decimal::from(50), Decimal::from(25));

    let quotes_long = strategy::rebate_mm::generate_quotes(&cfg, &market, &books, &risk);
    assert!(!quotes_long.is_empty());
    let (bid_l, _ask_l) = &quotes_long[0];

    // When long, bid should be lower or equal (less eager to buy more)
    assert!(bid_l.price <= bid_n.price, "long inventory should lower bid price: got {} vs baseline {}", bid_l.price, bid_n.price);
}
