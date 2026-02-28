use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::time;
use tracing::{error, info, warn};

use crate::auth::AuthClient;
use crate::config::Config;

/// Compact view of a tradeable market, distilled from the CLOB response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradableMarket {
    pub condition_id: String,
    pub question: String,
    pub tokens: Vec<TokenInfo>,
    pub neg_risk: bool,
    pub neg_risk_market_id: Option<String>,
    pub min_tick_size: Decimal,
    pub min_order_size: Decimal,
    pub maker_fee_bps: Decimal,
    pub rewards_active: bool,
    pub rewards_max_spread: Option<Decimal>,
    pub rewards_min_size: Option<Decimal>,
    pub volume_24h: f64,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenInfo {
    pub token_id: String,
    pub outcome: String,
    pub price: Decimal,
}

/// Filters applied during discovery to build the tradable universe.
fn passes_filter(
    active: bool,
    closed: bool,
    accepting_orders: bool,
    min_tick: Decimal,
) -> bool {
    active && !closed && accepting_orders && min_tick > Decimal::ZERO
}

/// Fetch all active markets from the CLOB endpoint, paginating through cursors.
async fn fetch_all_markets(client: &AuthClient, gamma_host: &str) -> Result<Vec<TradableMarket>> {
    let mut results = Vec::new();
    let mut cursor: Option<String> = Some("0".into());

    loop {
        let page = client.markets(cursor).await?;

        for m in &page.data {
            if !passes_filter(m.active, m.closed, m.accepting_orders, m.minimum_tick_size) {
                continue;
            }

            let condition_id = match &m.condition_id {
                Some(id) => format!("{id:?}"),
                None => continue,
            };

            let mut tokens: Vec<TokenInfo> = m
                .tokens
                .iter()
                .map(|t| TokenInfo {
                    token_id: t.token_id.to_string(),
                    outcome: t.outcome.clone(),
                    price: t.price,
                })
                .collect();

            // Exclude "Other" outcomes from neg-risk markets (catch-all bucket, not tradable)
            if m.neg_risk {
                tokens.retain(|t| t.outcome != "Other");
            }

            let rewards_max_spread = if m.rewards.max_spread > Decimal::ZERO {
                Some(m.rewards.max_spread)
            } else {
                None
            };
            let rewards_min_size = if m.rewards.min_size > Decimal::ZERO {
                Some(m.rewards.min_size)
            } else {
                None
            };

            results.push(TradableMarket {
                condition_id,
                question: m.question.clone(),
                tokens,
                neg_risk: m.neg_risk,
                neg_risk_market_id: m.neg_risk_market_id.map(|id| format!("{id:?}")),
                min_tick_size: m.minimum_tick_size,
                min_order_size: m.minimum_order_size,
                maker_fee_bps: m.maker_base_fee,
                rewards_active: m.rewards.rates.iter().any(|r| r.rewards_daily_rate > Decimal::ZERO),
                rewards_max_spread,
                rewards_min_size,
                // volume_24h not available from CLOB API; enriched via Gamma API later
                volume_24h: 0.0,
                tags: m.tags.clone(),
            });
        }

        // "DONE" sentinel or empty cursor means no more pages
        if page.next_cursor == "DONE" || page.next_cursor.is_empty() {
            break;
        }
        cursor = Some(page.next_cursor);
    }

    enrich_volume(gamma_host, &mut results).await;

    Ok(results)
}
///
/// TODO: actual Gamma API call pattern: `GET {gamma_host}/markets?id=<condition_id>`
/// For now this is a stub that can be wired up once the Gamma client is available.
pub async fn enrich_volume(_gamma_host: &str, _markets: &mut [TradableMarket]) {
    // Stubbed — each market's volume_24h stays at its current value.
    // Future: for each market, call GET /markets?id=<condition_id> and parse
    // the `volume24hr` field from the Gamma API response.
}

/// Spawn the discovery loop. Returns a watch receiver for the current universe.
pub fn spawn(
    config: &Config,
    client: AuthClient,
) -> (tokio::task::JoinHandle<()>, watch::Receiver<Arc<Vec<TradableMarket>>>) {
    let interval = config.discovery_interval;
    let gamma_host = config.gamma_host.clone();
    let (tx, rx) = watch::channel(Arc::new(Vec::new()));

    let handle = tokio::spawn(async move {
        let mut ticker = time::interval(interval);
        loop {
            ticker.tick().await;
            match fetch_all_markets(&client, &gamma_host).await {
                Ok(markets) => {
                    info!(count = markets.len(), "discovery: refreshed tradable universe");
                    let _ = tx.send(Arc::new(markets));
                }
                Err(e) => {
                    error!("discovery: fetch failed: {e}");
                }
            }
        }
    });

    (handle, rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn enrich_volume_empty_input_ok() {
        let mut markets: Vec<TradableMarket> = Vec::new();
        enrich_volume("https://gamma-api.polymarket.com", &mut markets).await;
        assert!(markets.is_empty());
    }
}
