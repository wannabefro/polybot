use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;
use tokio::time;
use tracing::{debug, error, warn};

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
    pub end_date: Option<DateTime<Utc>>,
}

impl TradableMarket {
    /// True when the market tags indicate a sports event.
    pub fn is_sports(&self) -> bool {
        self.tags.iter().any(|tag| {
            let lower = tag.to_lowercase();
            lower.contains("sports")
                || lower.contains("football")
                || lower.contains("basketball")
                || lower.contains("baseball")
                || lower.contains("hockey")
                || lower.contains("soccer")
                || lower.contains("tennis")
                || lower.contains("golf")
                || lower.contains("boxing")
                || lower.contains("mma")
                || lower.contains("nfl")
                || lower.contains("nba")
                || lower.contains("mlb")
                || lower.contains("nhl")
                || lower.contains("ncaa")
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenInfo {
    pub token_id: String,
    pub outcome: String,
    pub price: Decimal,
}

/// Filters applied during discovery to build the tradable universe.
fn passes_filter(active: bool, closed: bool, accepting_orders: bool, min_tick: Decimal) -> bool {
    active && !closed && accepting_orders && min_tick > Decimal::ZERO
}

/// Fetch all active markets from the CLOB endpoint, paginating through cursors.
async fn fetch_all_markets(client: &AuthClient, gamma_host: &str) -> Result<Vec<TradableMarket>> {
    let mut results = Vec::new();

    let mut stream = Box::pin(client.stream_data(|c, cursor| c.markets(cursor)));
    while let Some(maybe_market) = stream.next().await {
        let m = maybe_market?;
        if !passes_filter(m.active, m.closed, m.accepting_orders, m.minimum_tick_size) {
            continue;
        }

        let condition_id = match &m.condition_id {
            Some(id) => id.to_string(),
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
            neg_risk_market_id: m.neg_risk_market_id.map(|id| id.to_string()),
            min_tick_size: m.minimum_tick_size,
            min_order_size: m.minimum_order_size,
            maker_fee_bps: m.maker_base_fee,
            rewards_active: m
                .rewards
                .rates
                .iter()
                .any(|r| r.rewards_daily_rate > Decimal::ZERO),
            rewards_max_spread,
            rewards_min_size,
            // volume_24h not available from CLOB API; enriched via Gamma API later
            volume_24h: 0.0,
            tags: m.tags.clone(),
            end_date: m.end_date_iso,
        });
    }

    enrich_volume(gamma_host, &mut results).await;

    Ok(results)
}

fn parse_number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse::<f64>().ok())
}

fn extract_volume_24h(value: &Value) -> Option<f64> {
    const VOLUME_KEYS: [&str; 5] = [
        "volume24hr",
        "volume24h",
        "volume24Hr",
        "volume_24h",
        "volume",
    ];
    for key in VOLUME_KEYS {
        if let Some(v) = value.get(key).and_then(parse_number) {
            return Some(v);
        }
    }
    None
}

fn extract_condition_id(value: &Value) -> Option<&str> {
    const CONDITION_KEYS: [&str; 3] = ["conditionId", "condition_id", "id"];
    for key in CONDITION_KEYS {
        if let Some(v) = value.get(key).and_then(Value::as_str) {
            return Some(v);
        }
    }
    None
}

fn parse_volume_from_gamma_body(body: &Value, condition_id: &str) -> Option<f64> {
    match body {
        Value::Array(markets) => markets
            .iter()
            .find(|m| extract_condition_id(m) == Some(condition_id))
            .or_else(|| markets.first())
            .and_then(extract_volume_24h),
        Value::Object(_) => {
            if let Some(markets) = body.get("markets").and_then(Value::as_array) {
                return markets
                    .iter()
                    .find(|m| extract_condition_id(m) == Some(condition_id))
                    .or_else(|| markets.first())
                    .and_then(extract_volume_24h);
            }
            extract_volume_24h(body)
        }
        _ => None,
    }
}

/// Max markets to enrich via Gamma (avoid rate limits on 35k+ universe).
const GAMMA_ENRICH_LIMIT: usize = 600;
/// Concurrent Gamma requests (keep low to avoid 429s).
const GAMMA_CONCURRENCY: usize = 4;

pub async fn enrich_volume(gamma_host: &str, markets: &mut [TradableMarket]) {
    if markets.is_empty() {
        return;
    }

    let host = gamma_host.trim_end_matches('/').to_string();
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(err = %e, "discovery: failed to build gamma client");
            return;
        }
    };

    // Only enrich top N markets (sorted by rewards first since those are what we trade).
    let enrich_count = markets.len().min(GAMMA_ENRICH_LIMIT);
    let ids: Vec<(usize, String)> = markets[..enrich_count]
        .iter()
        .enumerate()
        .map(|(i, m)| (i, m.condition_id.clone()))
        .collect();

    let mut stream = futures::stream::iter(ids.into_iter().map(|(idx, condition_id)| {
        let client = client.clone();
        let host = host.clone();
        async move {
            let url = format!("{host}/markets");
            // Try once; on 429 back off and retry once.
            let resp = client
                .get(&url)
                .query(&[("id", condition_id.as_str())])
                .send()
                .await;
            let resp = match resp {
                Ok(r) if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    client
                        .get(&url)
                        .query(&[("id", condition_id.as_str())])
                        .send()
                        .await
                }
                other => other,
            };
            (idx, condition_id, resp)
        }
    }))
    .buffer_unordered(GAMMA_CONCURRENCY);

    let mut enriched = 0u32;
    let mut rate_limited = 0u32;
    while let Some((idx, condition_id, resp)) = stream.next().await {
        let Ok(resp) = resp else {
            continue;
        };
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            rate_limited += 1;
            continue;
        }
        if !resp.status().is_success() {
            continue;
        }

        match resp.json::<Value>().await {
            Ok(body) => {
                if let Some(volume) = parse_volume_from_gamma_body(&body, &condition_id) {
                    markets[idx].volume_24h = volume;
                    enriched += 1;
                }
            }
            Err(_) => {}
        }
    }
    if rate_limited > 0 {
        warn!(rate_limited, "discovery: gamma enrichment hit rate limits");
    }
    debug!(
        enriched,
        total = enrich_count,
        "discovery: volume enrichment complete"
    );
}

/// Spawn the discovery loop. Returns a watch receiver for the current universe.
pub fn spawn(
    config: &Config,
    client: AuthClient,
) -> (
    tokio::task::JoinHandle<()>,
    watch::Receiver<Arc<Vec<TradableMarket>>>,
) {
    let interval = config.discovery_interval;
    let gamma_host = config.gamma_host.clone();
    let (tx, rx) = watch::channel(Arc::new(Vec::new()));

    let handle = tokio::spawn(async move {
        let mut ticker = time::interval(interval);
        loop {
            ticker.tick().await;
            match fetch_all_markets(&client, &gamma_host).await {
                Ok(markets) => {
                    debug!(
                        count = markets.len(),
                        "discovery: refreshed tradable universe"
                    );
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

    #[test]
    fn parse_volume_handles_nested_markets_shape() {
        let body = serde_json::json!({
            "markets":[{"condition_id":"cond1","volume24h":42.0}]
        });
        assert_eq!(parse_volume_from_gamma_body(&body, "cond1"), Some(42.0));
    }
}
