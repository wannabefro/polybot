mod config;
mod geoblock;
mod auth;
mod market;
mod order;
mod risk;
mod strategy;
mod intelligence;
mod ops;

use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use polymarket_client_sdk::clob::types::Side;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use tokio::time;
use tracing::{debug, error, info, warn};

use crate::intelligence::signal;
use crate::market::book::BookStore;
use crate::market::ws::FeedEvent;
use crate::ops::metrics::Metrics;
use crate::ops::rewards::RewardTracker;
use crate::order::router::OrderRouter;
use crate::risk::guardrails::RiskEngine;
use crate::strategy::mean_revert::MeanRevertState;
use crate::strategy::reward::{HedgeTracker, UnhedgedFill, UnwindStage};

#[derive(Debug, Clone)]
struct ActiveQuote {
    order_id: String,
    placed_at: std::time::Instant,
    condition_id: String,
    intent: crate::order::pipeline::OrderIntent,
    needs_hedge: bool,
    price: Decimal,
    size: Decimal,
    tick_size: Decimal,
    is_unwind: bool,
    strategy_tag: &'static str,
}

#[derive(Debug, Clone)]
struct PendingMarkout {
    token_id: String,
    side: Side,
    fill_price: Decimal,
    tick_size: Decimal,
    at_5s: std::time::Instant,
    at_30s: std::time::Instant,
    done_5s: bool,
    done_30s: bool,
}

impl PendingMarkout {
    fn new(token_id: String, side: Side, fill_price: Decimal, tick_size: Decimal) -> Self {
        let now = std::time::Instant::now();
        Self {
            token_id,
            side,
            fill_price,
            tick_size: if tick_size > Decimal::ZERO { tick_size } else { dec!(0.01) },
            at_5s: now + Duration::from_secs(5),
            at_30s: now + Duration::from_secs(30),
            done_5s: false,
            done_30s: false,
        }
    }
}

fn quote_key(condition_id: &str, intent: &crate::order::pipeline::OrderIntent) -> String {
    format!("{condition_id}:{}:{:?}", intent.token_id, intent.side)
}

fn should_replace_quote(
    existing: &ActiveQuote,
    target_price: Decimal,
    target_size: Decimal,
    tick_size: Decimal,
    reprice_ticks: u32,
    refresh_age: Duration,
) -> bool {
    if existing.placed_at.elapsed() >= refresh_age {
        return true;
    }
    if existing.size != target_size {
        return true;
    }
    let price_step = tick_size * Decimal::from(reprice_ticks.max(1));
    (existing.price - target_price).abs() >= price_step
}

fn update_markouts(
    pending_markouts: &mut Vec<PendingMarkout>,
    books: &BookStore,
    metrics: &Metrics,
) {
    let now = std::time::Instant::now();
    pending_markouts.retain_mut(|entry| {
        if !entry.done_5s && now >= entry.at_5s {
            if let Some(mid) = books.get(&entry.token_id).and_then(|b| b.mid_price()) {
                let ticks = match entry.side {
                    Side::Buy => (mid - entry.fill_price) / entry.tick_size,
                    Side::Sell => (entry.fill_price - mid) / entry.tick_size,
                    _ => Decimal::ZERO,
                };
                metrics.add_markout_5s_ticks(ticks);
                entry.done_5s = true;
            }
        }
        if !entry.done_30s && now >= entry.at_30s {
            if let Some(mid) = books.get(&entry.token_id).and_then(|b| b.mid_price()) {
                let ticks = match entry.side {
                    Side::Buy => (mid - entry.fill_price) / entry.tick_size,
                    Side::Sell => (entry.fill_price - mid) / entry.tick_size,
                    _ => Decimal::ZERO,
                };
                metrics.add_markout_30s_ticks(ticks);
                entry.done_30s = true;
            }
        }
        !(entry.done_5s && entry.done_30s)
    });
}

fn scale_intent_size(
    intent: &mut crate::order::pipeline::OrderIntent,
    risk_multiplier: f64,
    min_size: Decimal,
) -> bool {
    let Some(mult) = Decimal::from_f64_retain(risk_multiplier) else {
        return false;
    };
    let size = intent.size * mult;
    if size < min_size || size <= Decimal::ZERO {
        return false;
    }
    intent.size = size;
    true
}

fn is_transient_order_rejection(msg: &str) -> bool {
    msg.contains("crosses book")
        || msg.contains("invalid post-only order")
        || msg.contains("not enough balance")
        || msg.contains("allowance")
}

async fn upsert_quote(
    router: &OrderRouter,
    books: &BookStore,
    risk_engine: &RiskEngine,
    hedge_tracker: &mut HedgeTracker,
    metrics: &Metrics,
    active_quotes: &mut HashMap<String, ActiveQuote>,
    pending_markouts: &mut Vec<PendingMarkout>,
    condition_id: &str,
    intent: &crate::order::pipeline::OrderIntent,
    needs_hedge: bool,
    tick_size: Decimal,
    strategy_tag: &'static str,
    refresh_age: Duration,
    reprice_ticks: u32,
) {
    let key = quote_key(condition_id, intent);

    if let Some(existing) = active_quotes.get(&key).cloned() {
        let replace = should_replace_quote(
            &existing,
            intent.price,
            intent.size,
            tick_size,
            reprice_ticks,
            refresh_age,
        );
        if !replace {
            return;
        }
        match router.cancel_batch(&[existing.order_id.clone()]).await {
            Ok(n) => {
                if n > 0 {
                    metrics.inc_quotes_cancelled();
                }
                metrics.inc_quote_replacements();
                active_quotes.remove(&key);
                risk_engine.reset_cancel_failures();
            }
            Err(e) => {
                error!(err = %e, key = %key, "quote replace cancel failed");
                risk_engine.record_cancel_failure();
                return;
            }
        }
    }

    match router.place(intent, books).await {
        Ok(result) => {
            metrics.inc_quotes_sent();
            process_fill(
                &result,
                intent,
                condition_id,
                risk_engine,
                hedge_tracker,
                metrics,
                needs_hedge,
                tick_size,
                pending_markouts,
            );
            if intent.post_only && result.paper_fill.is_none() && !result.live_matched {
                active_quotes.insert(
                    key,
                    ActiveQuote {
                        order_id: result.order_id,
                        placed_at: std::time::Instant::now(),
                        condition_id: condition_id.to_string(),
                        intent: intent.clone(),
                        needs_hedge,
                        price: intent.price,
                        size: intent.size,
                        tick_size,
                        is_unwind: false,
                        strategy_tag,
                    },
                );
            }
            risk_engine.reset_cancel_failures();
        }
        Err(e) => {
            let msg = e.to_string();
            if is_transient_order_rejection(&msg) {
                debug!(err = %e, strategy = strategy_tag, "quote place rejected (transient)");
            } else {
                error!(err = %e, strategy = strategy_tag, "quote place failed");
            }
        }
    }
}

/// Helper: process a PlaceResult, recording fills in risk engine and hedge tracker.
/// Works for both paper mode (paper_fill) and live mode (live_matched).
fn process_fill(
    result: &crate::order::router::PlaceResult,
    intent: &crate::order::pipeline::OrderIntent,
    condition_id: &str,
    risk_engine: &RiskEngine,
    hedge_tracker: &mut HedgeTracker,
    metrics: &Metrics,
    needs_hedge: bool,
    tick_size: Decimal,
    pending_markouts: &mut Vec<PendingMarkout>,
) {
    // Paper mode: use PaperFill details
    if let Some(ref fill) = result.paper_fill {
        let side = match fill.side.as_str() {
            "Buy" => Side::Buy,
            _ => Side::Sell,
        };
        risk_engine.record_fill(condition_id, &fill.token_id, side, fill.size, fill.notional);
        metrics.inc_fills();
        pending_markouts.push(PendingMarkout::new(
            fill.token_id.clone(),
            side,
            fill.price,
            tick_size,
        ));
        info!(
            "💰 FILL (paper) {} {} @ ${} (${:.2})",
            if matches!(side, Side::Buy) { "BUY" } else { "SELL" },
            fill.size, fill.price, fill.notional,
        );

        if needs_hedge {
            info!("⏱️ fill needs hedge — will unwind in 2min if complement doesn't fill");
            hedge_tracker.record_fill(UnhedgedFill {
                token_id: fill.token_id.clone(),
                condition_id: condition_id.to_string(),
                side,
                price: fill.price,
                size: fill.size,
                filled_at: std::time::Instant::now(),
                neg_risk: intent.neg_risk,
                fee_rate_bps: intent.fee_rate_bps,
                tick_size,
                unwind_attempts: 0,
                hard_stop_failures: 0,
            });
        }
        return;
    }

    // Live mode: CLOB reported a match (e.g. FOK/IOC filled)
    if result.live_matched {
        // Use actual fill amounts from CLOB, not intent amounts
        let size = if result.fill_size > Decimal::ZERO { result.fill_size } else { intent.size };
        let notional = if result.fill_notional > Decimal::ZERO { result.fill_notional } else { intent.price * intent.size };
        risk_engine.record_fill(condition_id, &intent.token_id, intent.side, size, notional);
        metrics.inc_fills();
        let fill_price = if !size.is_zero() {
            notional / size
        } else {
            intent.price
        };
        pending_markouts.push(PendingMarkout::new(
            intent.token_id.clone(),
            intent.side,
            fill_price,
            tick_size,
        ));
        let side_str = if matches!(intent.side, Side::Buy) { "BUY" } else { "SELL" };
        info!("💰 FILL {} {} @ ${} (${:.2})", side_str, size, intent.price, notional);

        if needs_hedge {
            info!("⏱️ fill needs hedge — will unwind in 2min if complement doesn't fill");
            hedge_tracker.record_fill(UnhedgedFill {
                token_id: intent.token_id.clone(),
                condition_id: condition_id.to_string(),
                side: intent.side,
                price: fill_price,
                size,
                filled_at: std::time::Instant::now(),
                neg_risk: intent.neg_risk,
                fee_rate_bps: intent.fee_rate_bps,
                tick_size,
                unwind_attempts: 0,
                hard_stop_failures: 0,
            });
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Install rustls crypto provider before any TLS connections.
    // Both ring and aws-lc-rs features are pulled in transitively;
    // we pick ring explicitly to avoid the ambiguity panic.
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "polybot=info".into()),
        )
        .compact()
        .with_target(false)
        .init();

    info!("polybot starting");

    // Load .env.local first (higher priority), then .env as fallback
    dotenvy::from_filename(".env.local").ok();
    dotenvy::dotenv().ok();

    // ── Phase 1: Config + compliance gate ──────────────────────
    let cfg = config::Config::from_env()?;
    geoblock::check_or_abort().await?;
    let (_geo_handle, mut geo_rx) = geoblock::spawn_monitor(&cfg);

    // ── Phase 2: Auth ──────────────────────────────────────────
    let auth_ctx = auth::init(&cfg).await?;

    // ── Phase 3: Market discovery + data feeds ─────────────────
    let (_disc_handle, universe_rx) =
        market::discovery::spawn(&cfg, auth_ctx.client.clone());
    let (_ws_handle, mut feed_rx) =
        market::ws::spawn(&cfg, universe_rx.clone());

    let books = BookStore::new();

    // ── Phase 4: Risk + intelligence ───────────────────────────
    let risk_engine = RiskEngine::new(cfg.clone());

    // Bootstrap risk engine with existing on-chain positions so
    // position recon doesn't halt on pre-existing inventory.
    {
        let address = format!("{:#x}", auth_ctx.signer.address());
        match risk::position::fetch_remote_positions(&address).await {
            Ok(positions) if !positions.is_empty() => {
                info!(
                    count = positions.len(),
                    "startup: seeding risk engine with {} existing positions",
                    positions.len()
                );
                risk_engine.seed_inventory(&positions);
            }
            Ok(_) => info!("startup: no existing positions found"),
            Err(e) => warn!(err = %e, "startup: could not fetch positions — recon may halt"),
        }
    }

    let rate_limiter = risk::rate_limit::RateLimiter::new(cfg.rate_limit_per_sec);
    let heartbeat = order::heartbeat::HeartbeatMonitor::new(cfg.heartbeat_interval);

    let (signal_tx, signal_rx) = signal::create();
    let _llm_handle = intelligence::llm::spawn(
        intelligence::llm::LlmConfig {
            poll_interval: cfg.llm_poll_interval,
            timeout: Duration::from_millis(1500),
            endpoint: std::env::var("POLYBOT_LLM_ENDPOINT").ok(),
        },
        signal_tx,
    );

    let _recon_handle = risk::position::spawn_recon(
        &cfg,
        auth_ctx.client.clone(),
        auth_ctx.signer.clone(),
        risk_engine.clone(),
    );

    // ── Phase 5: Order routing + metrics ───────────────────────
    let router = OrderRouter::new(&cfg, auth_ctx.clone());
    let metrics = Metrics::new();
    let reward_tracker = RewardTracker::new();

    // ── Startup reconciliation: cancel stale orders from prior run ──
    match router.cancel_all().await {
        Ok(()) => info!("startup: cancelled all pre-existing orders"),
        Err(e) => warn!(err = %e, "startup: cancel-all failed (may have no open orders)"),
    }

    // ── Refresh CLOB balance cache so orders aren't rejected ──
    {
        use polymarket_client_sdk::clob::types::AssetType;
        use polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest;
        let req = BalanceAllowanceRequest::builder()
            .asset_type(AssetType::Collateral)
            .build();
        match auth_ctx.client.update_balance_allowance(req).await {
            Ok(()) => info!("startup: CLOB balance cache refreshed"),
            Err(e) => warn!(err = %e, "startup: failed to refresh CLOB balance cache"),
        }
    }

    // ── Phase 6: Live NAV tracker ──────────────────────────────
    let (_nav_handle, mut nav_rx) = ops::nav::spawn(
        auth_ctx.client.clone(),
        cfg.nav_usdc,
        cfg.paper_mode,
        cfg.position_recon_interval, // reuse recon interval for NAV polls
    );

    // Strategy state
    let mut mean_revert = MeanRevertState::new();
    let mut hedge_tracker = HedgeTracker::new(cfg.hedge_timeout);
    let mut scalper_state = strategy::scalper::ScalperState::new();
    let mut active_quotes: HashMap<String, ActiveQuote> = HashMap::new();
    let mut pending_markouts: Vec<PendingMarkout> = Vec::new();
    let mut decay_tracker = strategy::decay::DecayTracker::new();
    let mut reward_enabled = cfg.nav_usdc >= cfg.reward_min_nav_usdc;
    let mut mean_revert_enabled = cfg.nav_usdc >= cfg.mean_revert_min_nav_usdc;
    let mut max_markets = cfg.max_active_markets();
    let mut live_nav = cfg.nav_usdc;
    let mut live_nav_initialized = false;
    let mut ws_first_quotable_logged = false;

    info!(
        paper_mode = cfg.paper_mode,
        nav = cfg.nav_usdc,
        small_account = cfg.is_small_account(),
        reward_enabled,
        mean_revert_enabled,
        max_active_markets = max_markets,
        "polybot ready — entering event loop"
    );

    // ── Main event loop ────────────────────────────────────────
    let mut quote_tick = time::interval(Duration::from_secs(cfg.quote_tick_secs));
    let mut daily_reset = time::interval(Duration::from_secs(86400));
    let mut status_tick = time::interval(cfg.metrics_interval);
    let mut rewards_tick = time::interval(Duration::from_secs(300)); // poll earnings every 5 min

    loop {
        tokio::select! {
            // ── Ctrl-C ──
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl-c — shutting down");
                if router.cancel_all().await.is_ok() {
                    active_quotes.clear();
                }
                break;
            }

            // ── Geoblock trip ──
            _ = geo_rx.changed() => {
                warn!("geoblock triggered — emergency cancel-all + shutdown");
                if router.cancel_all().await.is_ok() {
                    active_quotes.clear();
                }
                break;
            }

            // ── Live NAV update ──
            Ok(()) = nav_rx.changed() => {
                let snap = nav_rx.borrow().clone();
                let new_nav = snap.total_nav;
                // Always apply first real update (live_nav_initialized), then
                // only apply subsequent updates if delta >= $1.
                if !live_nav_initialized || (new_nav - live_nav).abs() >= 1.0 {
                    live_nav_initialized = true;
                    // Update risk limits
                    risk_engine.update_nav(new_nav, &cfg);

                    // Re-evaluate strategy gates and market caps
                    let mut nav_cfg = cfg.clone();
                    nav_cfg.nav_usdc = new_nav;
                    reward_enabled = new_nav >= cfg.reward_min_nav_usdc;
                    mean_revert_enabled = new_nav >= cfg.mean_revert_min_nav_usdc;
                    max_markets = nav_cfg.max_active_markets();

                    info!(
                        old_nav = format!("{live_nav:.2}"),
                        new_nav = format!("{new_nav:.2}"),
                        reward_enabled,
                        mean_revert_enabled,
                        max_active_markets = max_markets,
                        "nav: risk limits recalculated"
                    );
                    live_nav = new_nav;
                }
            }

            // ── WS book updates → BookStore ──
            Some(event) = feed_rx.recv() => {
                match event {
                    FeedEvent::BookSnapshot { asset_id, update } => {
                        let has_bids = !update.bids.is_empty();
                        let has_asks = !update.asks.is_empty();
                        books.apply(&asset_id, &update);
                        if has_bids && has_asks && !ws_first_quotable_logged {
                            info!(
                                asset_id = %asset_id,
                                bids = update.bids.len(),
                                asks = update.asks.len(),
                                "ws: first quotable book received"
                            );
                            ws_first_quotable_logged = true;
                        }
                        // After first successful update post-reconnect, resume quoting
                        if books.is_paused() {
                            books.resume();
                            info!("book: resumed quoting after resync");
                        }
                    }
                }
            }

            // ── Quoting tick ──
            _ = quote_tick.tick() => {
                let mids = books.all_mids();
                for (token_id, mid) in &mids {
                    if let Some(mid_f64) = mid.to_f64() {
                        scalper_state.record_mid(token_id, mid_f64);
                    }
                }
                let mtm_pnl = risk_engine.mark_to_market_pnl(&mids);
                risk_engine.set_daily_pnl(mtm_pnl);
                metrics.update_pnl(mtm_pnl);

                if risk_engine.is_halted() {
                    continue;
                }

                // Book paused (WS reconnect in progress) → skip quoting
                if books.is_paused() {
                    warn!("book: paused — skipping quoting tick");
                    continue;
                }

                // Check LLM signal
                let sig = signal::read(&signal_rx);
                if sig.pull {
                    warn!(reason = %sig.reason_code, "signal: pull — cancelling all");
                    if router.cancel_all().await.is_ok() {
                        active_quotes.clear();
                    }
                    continue;
                }
                let risk_multiplier = sig.effective_multiplier();

                // Check heartbeat health — Gap #4: cancel-all + halt on stale
                if heartbeat.is_stale() {
                    warn!("heartbeat stale — cancel-all + halt");
                    if let Err(e) = router.cancel_all().await {
                        error!(err = %e, "heartbeat cancel-all failed");
                        risk_engine.record_cancel_failure();
                    } else {
                        active_quotes.clear();
                    }
                    risk_engine.halt("stale heartbeat");
                    continue;
                }

                // Auto-resume from heartbeat halt once heartbeat is fresh again
                if risk_engine.is_halted() {
                    risk_engine.resume();
                }

                // ── Live fill detection: poll CLOB for resting order fills ──
                if !cfg.paper_mode && !active_quotes.is_empty() {
                    let order_ids: Vec<String> = active_quotes
                        .values()
                        .map(|q| q.order_id.clone())
                        .collect();
                    let detected = ops::fill_detect::detect_fills(&auth_ctx.client, &order_ids).await;
                    for fill in &detected {
                        // Find the active quote that matches this order
                        let Some((key, meta)) = active_quotes
                            .iter()
                            .find(|(_, q)| q.order_id == fill.order_id)
                            .map(|(k, q)| (k.clone(), q.clone()))
                        else {
                            continue;
                        };

                        let fill_size = fill.size_matched;
                        let fill_notional = fill.size_matched * meta.intent.price;
                        risk_engine.record_fill(
                            &meta.condition_id, &meta.intent.token_id,
                            meta.intent.side, fill_size, fill_notional,
                        );
                        metrics.inc_fills();
                        let side_str = if matches!(meta.intent.side, Side::Buy) { "BUY" } else { "SELL" };
                        info!(
                            "💰 FILL (detected) {} {} @ ${} (${:.2}) order={}",
                            side_str, fill_size, meta.intent.price, fill_notional, fill.order_id,
                        );

                        if meta.needs_hedge {
                            info!("⏱️ fill needs hedge — will unwind in 2min if complement doesn't fill");
                            hedge_tracker.record_fill(UnhedgedFill {
                                token_id: meta.intent.token_id.clone(),
                                condition_id: meta.condition_id.clone(),
                                side: meta.intent.side,
                                price: meta.intent.price,
                                size: fill_size,
                                filled_at: std::time::Instant::now(),
                                neg_risk: meta.intent.neg_risk,
                                fee_rate_bps: meta.intent.fee_rate_bps,
                                tick_size: meta.tick_size,
                                unwind_attempts: 0,
                                hard_stop_failures: 0,
                            });
                        }

                        pending_markouts.push(PendingMarkout::new(
                            meta.intent.token_id.clone(),
                            meta.intent.side,
                            meta.intent.price,
                            meta.tick_size,
                        ));
                        active_quotes.remove(&key);
                    }
                }

                // Unwind expired single-sided fills using a 2-minute staged FOK ladder.
                let stage1 = Duration::from_secs(cfg.unwind_stage1_secs);
                let stage2 = Duration::from_secs(cfg.unwind_stage2_secs);
                let hard_stop = Duration::from_secs(cfg.unwind_hard_stop_secs);
                for unwind in hedge_tracker.expired_unwinds(&books, stage1, stage2, hard_stop) {
                    metrics.inc_forced_unwinds();
                    let stage_label = match unwind.stage {
                        UnwindStage::Stage1 => "stage1",
                        UnwindStage::Stage2 => "stage2",
                        UnwindStage::HardStop => "hard-stop",
                    };
                    info!(
                        token = %unwind.token_id,
                        condition = %unwind.condition_id,
                        stage = stage_label,
                        price = %unwind.intent.price,
                        size = %unwind.intent.size,
                        "⚠️ UNWIND FOK submitted"
                    );
                    match router.place(&unwind.intent, &books).await {
                        Ok(result) => {
                            if result.paper_fill.is_some() || result.live_matched {
                                process_fill(
                                    &result,
                                    &unwind.intent,
                                    &unwind.condition_id,
                                    &risk_engine,
                                    &mut hedge_tracker,
                                    &metrics,
                                    false,
                                    dec!(0.01),
                                    &mut pending_markouts,
                                );
                                hedge_tracker.mark_hedged(&unwind.token_id);
                                info!("✅ UNWOUND {}", unwind.token_id);
                            } else if let Some((attempts, hard_failures)) =
                                hedge_tracker.record_unwind_attempt(&unwind.token_id, unwind.stage)
                            {
                                debug!(
                                    token = %unwind.token_id,
                                    stage = stage_label,
                                    attempts,
                                    hard_failures,
                                    "unwind FOK not filled, will retry"
                                );
                                if unwind.stage == UnwindStage::HardStop && hard_failures >= 6 {
                                    metrics.inc_unwind_hardstop_failures();
                                    scalper_state.set_condition_cooldown(
                                        &unwind.condition_id,
                                        Duration::from_secs(cfg.unwind_cooldown_secs),
                                    );
                                    risk_engine.halt("unwind hard-stop failure");
                                }
                            }
                            risk_engine.reset_cancel_failures();
                        }
                        Err(e) => {
                            error!(err = %e, token = %unwind.token_id, "unwind order failed");
                        }
                    }
                }

                // Cancel stale maker quotes before refreshing.
                // Protect complement quotes for unhedged fills — they need time to fill.
                let unhedged_conds = hedge_tracker.unhedged_conditions();
                let stale_quotes: Vec<(String, String)> = active_quotes
                    .iter()
                    .filter(|(_, q)| {
                        if q.placed_at.elapsed() < cfg.quote_max_age {
                            return false; // not stale yet
                        }
                        // If this quote's condition has an unhedged fill,
                        // protect it up to the hedge timeout
                        if unhedged_conds.contains(&q.condition_id)
                            && q.placed_at.elapsed() < cfg.hedge_timeout
                        {
                            return false; // protected complement
                        }
                        true
                    })
                    .map(|(k, q)| (k.clone(), q.order_id.clone()))
                    .collect();
                // Batch cancel stale quotes (single API call).
                let stale_keys: Vec<String> = stale_quotes.iter().map(|(k, _)| k.clone()).collect();
                let stale_ids: Vec<String> = stale_quotes.iter().map(|(_, id)| id.clone()).collect();
                if !stale_ids.is_empty() {
                    match router.cancel_batch(&stale_ids).await {
                        Ok(n) => {
                            for key in &stale_keys {
                                active_quotes.remove(key);
                            }
                            for _ in 0..n {
                                metrics.inc_quotes_cancelled();
                            }
                            risk_engine.reset_cancel_failures();
                        }
                        Err(e) => {
                            error!(err = %e, count = stale_ids.len(), "batch cancel failed");
                            risk_engine.record_cancel_failure();
                        }
                    }
                }

                let universe = universe_rx.borrow().clone();

                // ── Build neg-risk pair groups (Gap #6) ──
                // Group markets by neg_risk_market_id. Only quote if ALL tokens
                // in the pair have fresh books.
                let neg_risk_stale: std::collections::HashSet<String> = {
                    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
                    for market in universe.iter() {
                        if market.neg_risk {
                            if let Some(ref nrm_id) = market.neg_risk_market_id {
                                for tok in &market.tokens {
                                    groups.entry(nrm_id.clone())
                                        .or_default()
                                        .push(tok.token_id.clone());
                                }
                            }
                        }
                    }
                    let stale_threshold = Duration::from_secs(cfg.neg_risk_stale_secs);
                    let mut stale_set = std::collections::HashSet::new();
                    for (_, token_ids) in &groups {
                        let any_stale = token_ids.iter().any(|tid| {
                            books.get(tid).map_or(true, |b| b.is_stale(stale_threshold))
                        });
                        if any_stale {
                            for tid in token_ids {
                                stale_set.insert(tid.clone());
                            }
                        }
                    }
                    stale_set
                };

                // ── Rebate MM quoting (scalper-ranked + deterministic upsert) ──
                let mut nav_cfg = cfg.clone();
                nav_cfg.nav_usdc = live_nav;
                let rebate_market_limit = if nav_cfg.is_small_account() {
                    max_markets.min(cfg.scalper_small_max_markets)
                } else {
                    max_markets
                };
                let refresh_age = Duration::from_secs(cfg.scalper_refresh_secs);
                let mut ranked_rebate: Vec<(&crate::market::discovery::TradableMarket, f64)> = Vec::new();
                for market in universe.iter().filter(|m| !m.rewards_active) {
                    if unhedged_conds.contains(&market.condition_id) {
                        continue;
                    }
                    if market.neg_risk && market.tokens.iter().any(|t| neg_risk_stale.contains(&t.token_id)) {
                        continue;
                    }
                    if let Some(score) = strategy::scalper::evaluate_niche_market(
                        market,
                        &books,
                        &mut scalper_state,
                        &cfg,
                    ) {
                        ranked_rebate.push((market, score.score));
                    }
                }
                ranked_rebate.sort_by(|a, b| {
                    b.1.partial_cmp(&a.1)
                        .unwrap_or(Ordering::Equal)
                        .then(a.0.condition_id.cmp(&b.0.condition_id))
                });
                metrics.update_scalper_markets_selected(
                    ranked_rebate.len().min(rebate_market_limit) as u64,
                );

                for (market, _) in ranked_rebate.into_iter().take(rebate_market_limit) {
                    let quotes = strategy::rebate_mm::generate_quotes(&cfg, market, &books, &risk_engine);
                    for (mut bid, mut ask) in quotes {
                        if !scale_intent_size(&mut bid, risk_multiplier, market.min_order_size) {
                            continue;
                        }
                        if !scale_intent_size(&mut ask, risk_multiplier, market.min_order_size) {
                            continue;
                        }
                        if rate_limiter.try_acquire().is_err() {
                            break;
                        }
                        upsert_quote(
                            &router,
                            &books,
                            &risk_engine,
                            &mut hedge_tracker,
                            &metrics,
                            &mut active_quotes,
                            &mut pending_markouts,
                            &market.condition_id,
                            &bid,
                            true,
                            market.min_tick_size,
                            "rebate",
                            refresh_age,
                            cfg.scalper_reprice_ticks,
                        )
                        .await;
                        upsert_quote(
                            &router,
                            &books,
                            &risk_engine,
                            &mut hedge_tracker,
                            &metrics,
                            &mut active_quotes,
                            &mut pending_markouts,
                            &market.condition_id,
                            &ask,
                            true,
                            market.min_tick_size,
                            "rebate",
                            refresh_age,
                            cfg.scalper_reprice_ticks,
                        )
                        .await;
                    }
                }

                // ── Reward capture quoting (deterministic upsert) ──
                if reward_enabled {
                    let mut rw_count = 0usize;
                    let mut rw_empty_quotes = 0usize;
                    let rw_total_rewards_markets = universe.iter().filter(|m| m.rewards_active).count();
                    for market in universe.iter().filter(|m| m.rewards_active) {
                        if rw_count >= max_markets {
                            break;
                        }
                        if unhedged_conds.contains(&market.condition_id) {
                            continue;
                        }
                        if market.neg_risk && market.tokens.iter().any(|t| neg_risk_stale.contains(&t.token_id)) {
                            continue;
                        }
                        let has_book = market.tokens.iter().any(|t| books.get(&t.token_id).is_some());
                        if !has_book {
                            continue;
                        }

                        let quotes = strategy::reward::evaluate_reward_quote(
                            &cfg,
                            market,
                            &books,
                            &risk_engine,
                        );
                        rw_count += 1;
                        if quotes.is_empty() {
                            rw_empty_quotes += 1;
                        }

                        for (mut bid, mut ask) in quotes {
                            if !scale_intent_size(&mut bid, risk_multiplier, market.min_order_size) {
                                continue;
                            }
                            if !scale_intent_size(&mut ask, risk_multiplier, market.min_order_size) {
                                continue;
                            }
                            if rate_limiter.try_acquire().is_err() {
                                break;
                            }
                            upsert_quote(
                                &router,
                                &books,
                                &risk_engine,
                                &mut hedge_tracker,
                                &metrics,
                                &mut active_quotes,
                                &mut pending_markouts,
                                &market.condition_id,
                                &bid,
                                true,
                                market.min_tick_size,
                                "reward",
                                refresh_age,
                                cfg.scalper_reprice_ticks,
                            )
                            .await;
                            upsert_quote(
                                &router,
                                &books,
                                &risk_engine,
                                &mut hedge_tracker,
                                &metrics,
                                &mut active_quotes,
                                &mut pending_markouts,
                                &market.condition_id,
                                &ask,
                                true,
                                market.min_tick_size,
                                "reward",
                                refresh_age,
                                cfg.scalper_reprice_ticks,
                            )
                            .await;
                        }
                    }
                    if rw_count > 0 && rw_empty_quotes == rw_count {
                        debug!(
                            rewards_markets_total = rw_total_rewards_markets,
                            with_books = rw_count,
                            empty_quotes = rw_empty_quotes,
                            "reward: no quotable intents"
                        );
                    }
                }

                // Paper mode: simulate passive fills on resting quotes using latest books.
                if let Some(engine) = router.paper_engine() {
                    for fill in engine.simulate_resting_fills(&books) {
                        let Some((key, meta)) = active_quotes
                            .iter()
                            .find(|(_, q)| q.order_id == fill.paper_order_id)
                            .map(|(k, q)| (k.clone(), q.clone()))
                        else {
                            continue;
                        };

                        let result = crate::order::router::PlaceResult {
                            order_id: fill.paper_order_id.clone(),
                            paper_fill: Some(fill),
                            live_matched: false,
                            live_intent: None,
                            fill_size: Decimal::ZERO,
                            fill_notional: Decimal::ZERO,
                        };
                        process_fill(
                            &result,
                            &meta.intent,
                            &meta.condition_id,
                            &risk_engine,
                            &mut hedge_tracker,
                            &metrics,
                            meta.needs_hedge,
                            meta.tick_size,
                            &mut pending_markouts,
                        );
                        active_quotes.remove(&key);
                    }
                }

                update_markouts(&mut pending_markouts, &books, &metrics);

                // ── Mean reversion ──
                if mean_revert_enabled {
                    // Record prices for SMA
                    for market in universe.iter() {
                        for token in &market.tokens {
                            if let Some(book) = books.get(&token.token_id) {
                                if let Some(mid) = book.mid_price() {
                                    mean_revert.record_price(&token.token_id, mid);
                                }
                            }
                        }
                    }

                    // Check stop exits
                    for exit in mean_revert.positions_to_close(&books) {
                        info!(token = %exit.token_id, "mean-revert: stop triggered");
                        let Some(condition_id) = mean_revert
                            .open_positions()
                            .iter()
                            .find(|p| p.token_id == exit.token_id)
                            .map(|p| p.condition_id.clone())
                        else {
                            warn!(token = %exit.token_id, "mean-revert: missing condition id for exit");
                            continue;
                        };

                        match router.place(&exit, &books).await {
                            Ok(result) => {
                                process_fill(
                                    &result,
                                    &exit,
                                    &condition_id,
                                    &risk_engine,
                                    &mut hedge_tracker,
                                    &metrics,
                                    false,
                                    dec!(0.01),
                                    &mut pending_markouts,
                                );
                                if result.paper_fill.is_some() || result.live_matched {
                                    mean_revert.remove_position(&exit.token_id);
                                } else {
                                    warn!(token = %exit.token_id, "mean-revert: exit not filled; position retained");
                                }
                            }
                            Err(e) => {
                                error!(err = %e, "mean-revert exit failed");
                            }
                        }
                    }

                    // New entries
                    for market in universe.iter() {
                        if rate_limiter.try_acquire().is_err() {
                            break;
                        }
                        if let Some(mut intent) = strategy::mean_revert::evaluate_entry(
                            &cfg, market, &books, &risk_engine, &mean_revert,
                        ) {
                            if !scale_intent_size(&mut intent, risk_multiplier, market.min_order_size) {
                                continue;
                            }
                            match router.place(&intent, &books).await {
                                Ok(result) => {
                                    process_fill(
                                        &result,
                                        &intent,
                                        &market.condition_id,
                                        &risk_engine,
                                        &mut hedge_tracker,
                                        &metrics,
                                        false,
                                        market.min_tick_size,
                                        &mut pending_markouts,
                                    );
                                    // Only track position on confirmed fill, not just submission
                                    if result.paper_fill.is_some() || result.live_matched {
                                        mean_revert.open_position(
                                            strategy::mean_revert::MeanRevertPosition {
                                                condition_id: market.condition_id.clone(),
                                                token_id: intent.token_id.clone(),
                                                side: intent.side,
                                                entry_price: intent.price,
                                                size: intent.size,
                                                opened_at: std::time::Instant::now(),
                                                neg_risk: intent.neg_risk,
                                                fee_rate_bps: intent.fee_rate_bps,
                                            },
                                        );
                                    }
                                }
                                Err(e) => error!(err = %e, "mean-revert entry failed"),
                            }
                        }
                    }
                }

                // ── Time-decay strategy ──
                if cfg.decay_enabled {
                    // Cleanup resolved markets
                    decay_tracker.cleanup_resolved(&universe);

                    let candidates = strategy::decay::scan_candidates(
                        &universe, &books, &cfg, chrono::Utc::now(),
                    );
                    let mut decay_buys = 0u32;
                    for candidate in &candidates {
                        if rate_limiter.try_acquire().is_err() {
                            break;
                        }
                        let Some(intent) = strategy::decay::evaluate_decay_buy(
                            candidate, &cfg, &decay_tracker,
                        ) else {
                            continue;
                        };
                        match router.place(&intent, &books).await {
                            Ok(result) => {
                                let filled = result.paper_fill.is_some() || result.live_matched;
                                if filled {
                                    let fill_size = if let Some(ref pf) = result.paper_fill {
                                        pf.size
                                    } else {
                                        result.fill_size
                                    };
                                    decay_tracker.record_fill(
                                        &candidate.condition_id,
                                        &candidate.token_id,
                                        &candidate.outcome,
                                        fill_size,
                                        intent.price,
                                        intent.neg_risk,
                                        intent.fee_rate_bps,
                                    );
                                    decay_buys += 1;
                                    info!(
                                        outcome = %candidate.outcome,
                                        price = %intent.price,
                                        size = %fill_size,
                                        hours_to_end = candidate.hours_to_end,
                                        "🕐 decay: bought shares"
                                    );
                                } else {
                                    info!(
                                        outcome = %candidate.outcome,
                                        price = %intent.price,
                                        size = %intent.size,
                                        "🕐 decay: FOK not filled (no liquidity at price)"
                                    );
                                }
                            }
                            Err(e) => {
                                let msg = e.to_string();
                                if is_transient_order_rejection(&msg) {
                                    debug!(err = %e, "decay place rejected (transient)");
                                } else {
                                    error!(err = %e, "decay place failed");
                                }
                            }
                        }
                    }
                    if decay_buys > 0 {
                        debug!(
                            buys = decay_buys,
                            deployed = %decay_tracker.deployed_capital(),
                            positions = decay_tracker.position_count(),
                            "decay: tick summary"
                        );
                    }
                }

                let quote_age_ms = active_quotes
                    .values()
                    .map(|q| q.placed_at.elapsed().as_millis() as u64)
                    .max()
                    .unwrap_or(0);
                metrics.update_quote_age_ms(quote_age_ms);

                // Record heartbeat on successful tick
                heartbeat.record_success();
            }

            // ── Daily reset ──
            _ = daily_reset.tick() => {
                info!("daily reset: clearing metrics + risk counters");
                metrics.reset_daily();
                risk_engine.reset_daily();
            }

            // ── Reward earnings poll ──
            _ = rewards_tick.tick() => {
                if !cfg.paper_mode {
                    ops::rewards::refresh_earnings(&auth_ctx.client, &reward_tracker).await;
                }
            }

            // ── Status dashboard ──
            _ = status_tick.tick() => {
                let universe = universe_rx.borrow().clone();
                let s = metrics.snapshot();
                let books_quotable = books.count_quotable();
                let halted = risk_engine.is_halted();
                let rewards = reward_tracker.total();
                let decay_deployed = decay_tracker.deployed_capital();
                let decay_count = decay_tracker.position_count();

                info!(
                    "📊 NAV ${:.2} | quotes {}/{} | fills {} | pnl {} | rewards ${:.2} | scalper mkt {} | repl {} | unwinds {} | hardfails {} | mk5 {} | mk30 {} | decay ${:.2}({}) | markets {} | books {} | {}",
                    live_nav,
                    active_quotes.len(),
                    s.quotes_sent,
                    s.fills_count,
                    s.daily_pnl,
                    rewards,
                    s.scalper_markets_selected,
                    s.quote_replacements,
                    s.forced_unwinds,
                    s.unwind_hardstop_failures,
                    s.markout_5s_ticks_total,
                    s.markout_30s_ticks_total,
                    decay_deployed,
                    decay_count,
                    universe.len(),
                    books_quotable,
                    if halted { "⛔ HALTED" } else { "✅ OK" },
                );
            }
        }
    }

    info!("polybot shutdown complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymarket_client_sdk::clob::types::OrderType;
    use rust_decimal_macros::dec;

    fn test_quote(age_secs: u64, price: Decimal, size: Decimal) -> ActiveQuote {
        ActiveQuote {
            order_id: "o1".into(),
            placed_at: std::time::Instant::now() - Duration::from_secs(age_secs),
            condition_id: "cond1".into(),
            intent: crate::order::pipeline::OrderIntent {
                token_id: "t1".into(),
                side: Side::Buy,
                price,
                size,
                order_type: OrderType::GTC,
                post_only: true,
                neg_risk: false,
                fee_rate_bps: Decimal::ZERO,
            },
            needs_hedge: true,
            price,
            size,
            tick_size: dec!(0.01),
            is_unwind: false,
            strategy_tag: "rebate",
        }
    }

    #[test]
    fn no_replace_within_threshold_and_ttl() {
        let existing = test_quote(5, dec!(0.50), dec!(10));
        assert!(!should_replace_quote(
            &existing,
            dec!(0.50),
            dec!(10),
            dec!(0.01),
            1,
            Duration::from_secs(15),
        ));
    }

    #[test]
    fn replace_on_one_tick_move() {
        let existing = test_quote(5, dec!(0.50), dec!(10));
        assert!(should_replace_quote(
            &existing,
            dec!(0.51),
            dec!(10),
            dec!(0.01),
            1,
            Duration::from_secs(15),
        ));
    }

    #[test]
    fn replace_on_ttl_expiry() {
        let existing = test_quote(16, dec!(0.50), dec!(10));
        assert!(should_replace_quote(
            &existing,
            dec!(0.50),
            dec!(10),
            dec!(0.01),
            1,
            Duration::from_secs(15),
        ));
    }

    #[test]
    fn replace_on_size_change() {
        let existing = test_quote(5, dec!(0.50), dec!(10));
        assert!(should_replace_quote(
            &existing,
            dec!(0.50),
            dec!(12),
            dec!(0.01),
            1,
            Duration::from_secs(15),
        ));
    }
}
