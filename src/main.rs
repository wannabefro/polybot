mod config;
mod geoblock;
mod auth;
mod market;
mod order;
mod risk;
mod strategy;
mod intelligence;
mod ops;

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use polymarket_client_sdk::clob::types::Side;
use rust_decimal::Decimal;
use tokio::time;
use tracing::{error, info, warn};

use crate::intelligence::signal;
use crate::market::book::BookStore;
use crate::market::ws::FeedEvent;
use crate::ops::metrics::Metrics;
use crate::order::router::OrderRouter;
use crate::risk::guardrails::RiskEngine;
use crate::strategy::mean_revert::MeanRevertState;
use crate::strategy::reward::{HedgeTracker, UnhedgedFill};

#[derive(Debug, Clone)]
struct ActiveQuote {
    order_id: String,
    placed_at: std::time::Instant,
}

fn quote_key(intent: &crate::order::pipeline::OrderIntent) -> String {
    format!("{}:{:?}", intent.token_id, intent.side)
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
) {
    // Paper mode: use PaperFill details
    if let Some(ref fill) = result.paper_fill {
        let side = match fill.side.as_str() {
            "Buy" => Side::Buy,
            _ => Side::Sell,
        };
        risk_engine.record_fill(condition_id, &fill.token_id, side, fill.size, fill.notional);
        metrics.inc_fills();

        if needs_hedge {
            hedge_tracker.record_fill(UnhedgedFill {
                token_id: fill.token_id.clone(),
                side,
                price: fill.price,
                size: fill.size,
                filled_at: std::time::Instant::now(),
                neg_risk: intent.neg_risk,
                fee_rate_bps: intent.fee_rate_bps,
            });
        }
        return;
    }

    // Live mode: CLOB reported a match (e.g. FOK/IOC filled)
    if result.live_matched {
        let notional = intent.price * intent.size;
        risk_engine.record_fill(condition_id, &intent.token_id, intent.side, intent.size, notional);
        metrics.inc_fills();

        if needs_hedge {
            hedge_tracker.record_fill(UnhedgedFill {
                token_id: intent.token_id.clone(),
                side: intent.side,
                price: intent.price,
                size: intent.size,
                filled_at: std::time::Instant::now(),
                neg_risk: intent.neg_risk,
                fee_rate_bps: intent.fee_rate_bps,
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
        .json()
        .init();

    info!("polybot starting");

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
    let router = OrderRouter::new(&cfg, auth_ctx);
    let metrics = Metrics::new();

    // Strategy state
    let mut mean_revert = MeanRevertState::new();
    let mut hedge_tracker = HedgeTracker::new(cfg.hedge_timeout);
    let mut active_quotes: HashMap<String, ActiveQuote> = HashMap::new();

    info!(
        paper_mode = cfg.paper_mode,
        nav = cfg.nav_usdc,
        "polybot ready — entering event loop"
    );

    // ── Main event loop ────────────────────────────────────────
    let mut quote_tick = time::interval(Duration::from_secs(cfg.quote_tick_secs));
    let mut daily_reset = time::interval(Duration::from_secs(86400));
    let mut status_tick = time::interval(cfg.metrics_interval);

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

            // ── WS book updates → BookStore ──
            Some(event) = feed_rx.recv() => {
                match event {
                    FeedEvent::BookSnapshot { asset_id, update } => {
                        books.apply(&asset_id, &update);
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

                // Handle hedge emergencies first
                if hedge_tracker.has_emergency() {
                    error!("hedge SLA breached — cancel-all + flatten");
                    if let Err(e) = router.cancel_all().await {
                        error!(err = %e, "hedge emergency cancel-all failed");
                        risk_engine.record_cancel_failure();
                    } else {
                        active_quotes.clear();
                    }
                    risk_engine.halt("hedge SLA breach");
                    hedge_tracker.clear();
                    continue;
                }

                // Pending hedges
                for hedge in hedge_tracker.pending_hedges(&books) {
                    match router.place(&hedge, &books).await {
                        Ok(result) => {
                            if result.paper_fill.is_some() || result.live_matched {
                                hedge_tracker.mark_hedged(&hedge.token_id);
                            }
                            risk_engine.reset_cancel_failures();
                        }
                        Err(e) => {
                            error!(err = %e, "hedge order failed");
                        }
                    }
                }

                // Cancel stale maker quotes before refreshing.
                let stale_quotes: Vec<(String, String)> = active_quotes
                    .iter()
                    .filter(|(_, q)| q.placed_at.elapsed() >= cfg.quote_max_age)
                    .map(|(k, q)| (k.clone(), q.order_id.clone()))
                    .collect();
                for (key, order_id) in stale_quotes {
                    match router.cancel(&order_id).await {
                        Ok(()) => {
                            active_quotes.remove(&key);
                            metrics.inc_quotes_cancelled();
                            risk_engine.reset_cancel_failures();
                        }
                        Err(e) => {
                            error!(err = %e, order_id = %order_id, "quote lifecycle cancel failed");
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

                // ── Rebate MM quoting (concurrent dispatch) ──
                {
                    let mut rebate_intents: Vec<(crate::order::pipeline::OrderIntent, String, bool, String)> = Vec::new();
                    for market in universe.iter() {
                        // Skip rewards-active markets — reward capture handles them
                        if market.rewards_active {
                            continue;
                        }
                        if market.neg_risk && market.tokens.iter().any(|t| neg_risk_stale.contains(&t.token_id)) {
                            continue;
                        }
                        let quotes = strategy::rebate_mm::generate_quotes(
                            &cfg, market, &books, &risk_engine,
                        );
                        for (mut bid, mut ask) in quotes {
                            if !scale_intent_size(&mut bid, risk_multiplier, market.min_order_size) {
                                continue;
                            }
                            if !scale_intent_size(&mut ask, risk_multiplier, market.min_order_size) {
                                continue;
                            }
                            let bid_key = quote_key(&bid);
                            let ask_key = quote_key(&ask);
                            if active_quotes.contains_key(&bid_key) || active_quotes.contains_key(&ask_key) {
                                continue;
                            }
                            // Rate-limit per order pair, not per market
                            if rate_limiter.try_acquire().is_err() {
                                break;
                            }
                            rebate_intents.push((bid, market.condition_id.clone(), false, bid_key));
                            rebate_intents.push((ask, market.condition_id.clone(), false, ask_key));
                        }
                    }
                    let results: Vec<_> = futures::future::join_all(
                        rebate_intents.iter().map(|(intent, _, _, _)| router.place(intent, &books))
                    ).await;
                    for (res, (intent, cond_id, needs_hedge, key)) in results.into_iter().zip(rebate_intents.iter()) {
                        match res {
                            Ok(result) => {
                                metrics.inc_quotes_sent();
                                process_fill(&result, intent, cond_id, &risk_engine, &mut hedge_tracker, &metrics, *needs_hedge);
                                if intent.post_only && result.paper_fill.is_none() && !result.live_matched {
                                    active_quotes.insert(
                                        key.clone(),
                                        ActiveQuote {
                                            order_id: result.order_id,
                                            placed_at: std::time::Instant::now(),
                                        },
                                    );
                                }
                                risk_engine.reset_cancel_failures();
                            }
                            Err(e) => error!(err = %e, "rebate-mm place failed"),
                        }
                    }
                }

                // ── Reward capture quoting (concurrent dispatch) ──
                {
                    let mut reward_intents: Vec<(crate::order::pipeline::OrderIntent, String, bool, String)> = Vec::new();
                    for market in universe.iter().filter(|m| m.rewards_active) {
                        if market.neg_risk && market.tokens.iter().any(|t| neg_risk_stale.contains(&t.token_id)) {
                            continue;
                        }
                        let quotes = strategy::reward::evaluate_reward_quote(
                            &cfg, market, &books, &risk_engine,
                        );
                        for (mut bid, mut ask) in quotes {
                            if !scale_intent_size(&mut bid, risk_multiplier, market.min_order_size) {
                                continue;
                            }
                            if !scale_intent_size(&mut ask, risk_multiplier, market.min_order_size) {
                                continue;
                            }
                            let bid_key = quote_key(&bid);
                            let ask_key = quote_key(&ask);
                            if active_quotes.contains_key(&bid_key) || active_quotes.contains_key(&ask_key) {
                                continue;
                            }
                            // Rate-limit per order pair, not per market
                            if rate_limiter.try_acquire().is_err() {
                                break;
                            }
                            reward_intents.push((bid, market.condition_id.clone(), true, bid_key));
                            reward_intents.push((ask, market.condition_id.clone(), true, ask_key));
                        }
                    }
                    let results: Vec<_> = futures::future::join_all(
                        reward_intents.iter().map(|(intent, _, _, _)| router.place(intent, &books))
                    ).await;
                    for (res, (intent, cond_id, needs_hedge, key)) in results.into_iter().zip(reward_intents.iter()) {
                        match res {
                            Ok(result) => {
                                metrics.inc_quotes_sent();
                                process_fill(&result, intent, cond_id, &risk_engine, &mut hedge_tracker, &metrics, *needs_hedge);
                                if intent.post_only && result.paper_fill.is_none() && !result.live_matched {
                                    active_quotes.insert(
                                        key.clone(),
                                        ActiveQuote {
                                            order_id: result.order_id,
                                            placed_at: std::time::Instant::now(),
                                        },
                                    );
                                }
                                risk_engine.reset_cancel_failures();
                            }
                            Err(e) => error!(err = %e, "reward place failed"),
                        }
                    }
                }

                // ── Mean reversion ──
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

            // ── Status dashboard ──
            _ = status_tick.tick() => {
                let universe = universe_rx.borrow().clone();
                let s = metrics.snapshot();
                let books_populated = universe.iter()
                    .flat_map(|m| m.tokens.iter())
                    .filter(|t| books.get(&t.token_id).is_some())
                    .count();
                let books_total = universe.iter()
                    .map(|m| m.tokens.len())
                    .sum::<usize>();
                let mr_positions = mean_revert.open_positions().len();
                let unhedged = hedge_tracker.unhedged_count();
                let halted = risk_engine.is_halted();

                info!(
                    markets = universe.len(),
                    books = %format!("{}/{}", books_populated, books_total),
                    active_quotes = active_quotes.len(),
                    quotes_sent = s.quotes_sent,
                    fills = s.fills_count,
                    pnl = %s.daily_pnl,
                    rebate = %s.rebate_accrual,
                    mr_positions,
                    unhedged,
                    risk_rejections = s.risk_rejections,
                    cancel_failures = s.cancel_failures,
                    ws_reconnects = s.ws_reconnects,
                    throttled = s.throttle_count,
                    halted,
                    "📊 status"
                );
            }
        }
    }

    info!("polybot shutdown complete");
    Ok(())
}
