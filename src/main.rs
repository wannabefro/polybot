mod config;
mod geoblock;
mod auth;
mod market;
mod order;
mod risk;
mod strategy;
mod intelligence;
mod ops;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use rust_decimal_macros::dec;
use tokio::time;
use tracing::{error, info, warn};

use crate::intelligence::signal;
use crate::market::book::BookStore;
use crate::market::ws::FeedEvent;
use crate::ops::metrics::Metrics;
use crate::order::router::OrderRouter;
use crate::risk::guardrails::{RiskEngine, RiskVerdict};
use crate::strategy::mean_revert::MeanRevertState;
use crate::strategy::reward::HedgeTracker;

#[tokio::main]
async fn main() -> Result<()> {
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
    let rate_limiter = risk::rate_limit::RateLimiter::new(70.0);
    let heartbeat = order::heartbeat::HeartbeatMonitor::new(cfg.heartbeat_interval);

    let (signal_tx, signal_rx) = signal::create();
    let _llm_handle = intelligence::llm::spawn(
        intelligence::llm::LlmConfig {
            poll_interval: Duration::from_secs(10),
            timeout: Duration::from_millis(1500),
            endpoint: std::env::var("POLYBOT_LLM_ENDPOINT").ok(),
        },
        signal_tx,
    );

    let _recon_handle = risk::position::spawn_recon(
        &cfg,
        auth_ctx.client.clone(),
        risk_engine.clone(),
    );

    // ── Phase 5: Order routing + metrics ───────────────────────
    let router = OrderRouter::new(&cfg, auth_ctx);
    let metrics = Metrics::new();
    let _metrics_handle = ops::metrics::spawn_logger(metrics.clone(), Duration::from_secs(30));

    // Strategy state
    let mut mean_revert = MeanRevertState::new();
    let mut hedge_tracker = HedgeTracker::new(cfg.hedge_timeout);

    info!(
        paper_mode = cfg.paper_mode,
        nav = cfg.nav_usdc,
        "polybot ready — entering event loop"
    );

    // ── Main event loop ────────────────────────────────────────
    let mut quote_tick = time::interval(Duration::from_secs(5));
    let mut daily_reset = time::interval(Duration::from_secs(86400));

    loop {
        tokio::select! {
            // ── Ctrl-C ──
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl-c — shutting down");
                let _ = router.cancel_all().await;
                break;
            }

            // ── Geoblock trip ──
            _ = geo_rx.changed() => {
                warn!("geoblock triggered — emergency cancel-all + shutdown");
                let _ = router.cancel_all().await;
                break;
            }

            // ── WS book updates → BookStore ──
            Some(event) = feed_rx.recv() => {
                match event {
                    FeedEvent::BookSnapshot { asset_id, update } => {
                        books.apply(&asset_id, &update);
                    }
                }
            }

            // ── Quoting tick ──
            _ = quote_tick.tick() => {
                if risk_engine.is_halted() {
                    continue;
                }

                // Check LLM signal
                let sig = signal::read(&signal_rx);
                if sig.pull {
                    warn!(reason = %sig.reason_code, "signal: pull — cancelling all");
                    let _ = router.cancel_all().await;
                    continue;
                }

                // Check heartbeat health
                if heartbeat.is_stale() {
                    warn!("heartbeat stale — skipping quoting tick");
                    continue;
                }

                // Handle hedge emergencies first
                if hedge_tracker.has_emergency() {
                    error!("hedge SLA breached — cancel-all + flatten");
                    let _ = router.cancel_all().await;
                    hedge_tracker.clear();
                    metrics.inc_cancel_failures();
                    continue;
                }

                // Pending hedges
                for hedge in hedge_tracker.pending_hedges(&books) {
                    if let Err(e) = router.place(&hedge, &books).await {
                        error!(err = %e, "hedge order failed");
                    }
                }

                let universe = universe_rx.borrow().clone();

                // ── Rebate MM quoting ──
                for market in universe.iter() {
                    if rate_limiter.try_acquire().is_err() {
                        break;
                    }
                    if let Some((bid, ask)) = strategy::rebate_mm::generate_quotes(
                        &cfg, market, &books, &risk_engine,
                    ) {
                        match router.place(&bid, &books).await {
                            Ok(_) => metrics.inc_quotes_sent(),
                            Err(e) => error!(err = %e, "bid place failed"),
                        }
                        match router.place(&ask, &books).await {
                            Ok(_) => metrics.inc_quotes_sent(),
                            Err(e) => error!(err = %e, "ask place failed"),
                        }
                    }
                }

                // ── Reward capture quoting ──
                for market in universe.iter().filter(|m| m.rewards_active) {
                    if rate_limiter.try_acquire().is_err() {
                        break;
                    }
                    if let Some((bid, ask)) = strategy::reward::evaluate_reward_quote(
                        &cfg, market, &books, &risk_engine,
                    ) {
                        match router.place(&bid, &books).await {
                            Ok(_) => metrics.inc_quotes_sent(),
                            Err(e) => error!(err = %e, "reward bid failed"),
                        }
                        match router.place(&ask, &books).await {
                            Ok(_) => metrics.inc_quotes_sent(),
                            Err(e) => error!(err = %e, "reward ask failed"),
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
                    if let Err(e) = router.place(&exit, &books).await {
                        error!(err = %e, "mean-revert exit failed");
                    }
                    mean_revert.remove_position(&exit.token_id);
                }

                // New entries
                for market in universe.iter() {
                    if rate_limiter.try_acquire().is_err() {
                        break;
                    }
                    if let Some(intent) = strategy::mean_revert::evaluate_entry(
                        &cfg, market, &books, &risk_engine, &mean_revert,
                    ) {
                        match router.place(&intent, &books).await {
                            Ok(_) => {
                                mean_revert.open_position(
                                    strategy::mean_revert::MeanRevertPosition {
                                        token_id: intent.token_id.clone(),
                                        side: intent.side,
                                        entry_price: intent.price,
                                        size: intent.size,
                                        opened_at: std::time::Instant::now(),
                                    },
                                );
                                metrics.inc_fills();
                            }
                            Err(e) => error!(err = %e, "mean-revert entry failed"),
                        }
                    }
                }

                // Record heartbeat on successful tick
                heartbeat.record_success();
            }

            // ── Daily reset ──
            _ = daily_reset.tick() => {
                info!("daily reset: clearing metrics + risk counters");
                metrics.reset_daily();
                risk_engine.reset_daily();
            }
        }
    }

    info!("polybot shutdown complete");
    Ok(())
}
