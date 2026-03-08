use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use tokio::time::{self, Duration};
use tracing::{debug, error, warn};

use crate::auth::{AuthClient, Signer};
use crate::config::Config;
use crate::risk::guardrails::RiskEngine;

/// Snapshot of reconciled positions.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct PositionSnapshot {
    /// condition_id → net size
    pub positions: HashMap<String, Decimal>,
    /// Total unrealized P&L across all positions.
    pub unrealized_pnl: Decimal,
}

/// Maximum acceptable mismatch between local and remote positions (% of NAV).
const RECON_MISMATCH_THRESHOLD_PCT: f64 = 0.05; // 5% NAV
const POSITION_FETCH_ATTEMPTS: usize = 3;
const POSITION_FETCH_BACKOFF_MS: u64 = 250;
const MISMATCH_CONFIRMATION_ATTEMPTS: usize = 3;
const MISMATCH_CONFIRMATION_BACKOFF: Duration = Duration::from_secs(2);

/// A position entry from the data API, including condition_id.
#[derive(Debug, Clone)]
pub struct RemotePosition {
    pub token_id: String,
    pub condition_id: String,
    pub size: Decimal,
}

/// Fetch positions from the Polymarket data API (token_id → size only).
pub async fn fetch_remote_positions(address: &str) -> Result<HashMap<String, Decimal>> {
    let entries = fetch_remote_positions_full(address).await?;
    Ok(entries.into_iter().map(|p| (p.token_id, p.size)).collect())
}

/// Fetch positions with full metadata (token_id, condition_id, size).
pub async fn fetch_remote_positions_full(address: &str) -> Result<Vec<RemotePosition>> {
    let client = reqwest::Client::new();
    let data_host = data_host();
    fetch_remote_positions_full_from_host(&client, &data_host, address).await
}

fn data_host() -> String {
    std::env::var("POLYBOT_DATA_HOST").unwrap_or_else(|_| "https://data-api.polymarket.com".into())
}

fn fetch_backoff(attempt: usize) -> Duration {
    Duration::from_millis(POSITION_FETCH_BACKOFF_MS * (1_u64 << attempt.saturating_sub(1)))
}

async fn fetch_remote_positions_from_host(
    client: &reqwest::Client,
    data_host: &str,
    address: &str,
) -> Result<HashMap<String, Decimal>> {
    let entries = fetch_remote_positions_full_from_host(client, data_host, address).await?;
    Ok(entries.into_iter().map(|p| (p.token_id, p.size)).collect())
}

async fn fetch_remote_positions_full_from_host(
    client: &reqwest::Client,
    data_host: &str,
    address: &str,
) -> Result<Vec<RemotePosition>> {
    let url = format!("{}/positions?user={}", data_host, address);

    for attempt in 1..=POSITION_FETCH_ATTEMPTS {
        match fetch_remote_positions_full_once(client, &url).await {
            Ok(positions) => return Ok(positions),
            Err(e) if attempt < POSITION_FETCH_ATTEMPTS => {
                let backoff = fetch_backoff(attempt);
                warn!(
                    attempt,
                    max_attempts = POSITION_FETCH_ATTEMPTS,
                    backoff_ms = backoff.as_millis(),
                    err = %e,
                    "position-recon: retrying data API fetch"
                );
                time::sleep(backoff).await;
            }
            Err(e) => return Err(e),
        }
    }

    unreachable!("position fetch loop returns on success or final error");
}

async fn fetch_remote_positions_full_once(
    client: &reqwest::Client,
    url: &str,
) -> Result<Vec<RemotePosition>> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PosEntry {
        asset: Option<String>,
        condition_id: Option<String>,
        size: Option<f64>,
    }

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to reach data API at {url}"))?;

    if !response.status().is_success() {
        warn!(status = %response.status(), "position-recon: data API returned error");
        return Err(anyhow::anyhow!("data API error: {}", response.status()));
    }

    let body = response
        .text()
        .await
        .with_context(|| format!("failed reading data API body from {url}"))?;
    let body_preview: String = body.chars().take(256).collect();
    let entries: Vec<PosEntry> = serde_json::from_str(&body).with_context(|| {
        format!("failed to parse data API JSON from {url}; body preview: {body_preview}")
    })?;

    let mut result = Vec::new();
    for entry in entries {
        if let (Some(asset), Some(cid), Some(size)) = (entry.asset, entry.condition_id, entry.size)
        {
            if let Some(d) = Decimal::from_f64_retain(size) {
                if d > Decimal::ZERO {
                    result.push(RemotePosition {
                        token_id: asset,
                        condition_id: cid,
                        size: d,
                    });
                }
            }
        }
    }

    Ok(result)
}

async fn confirm_persistent_mismatch(
    client: &reqwest::Client,
    data_host: &str,
    address: &str,
    local: &HashMap<String, Decimal>,
    threshold: Decimal,
    initial_mismatch: Decimal,
) -> Result<Option<Decimal>> {
    warn!(
        mismatch = %initial_mismatch,
        threshold = %threshold,
        confirmations = MISMATCH_CONFIRMATION_ATTEMPTS,
        "position-recon: mismatch detected; starting confirmation checks"
    );

    let mut mismatch = initial_mismatch;
    for attempt in 2..=MISMATCH_CONFIRMATION_ATTEMPTS {
        time::sleep(MISMATCH_CONFIRMATION_BACKOFF).await;
        let remote = match fetch_remote_positions_from_host(client, data_host, address).await {
            Ok(remote) => remote,
            Err(e) => {
                warn!(
                    attempt,
                    err = %e,
                    "position-recon: mismatch confirmation fetch failed; skipping halt"
                );
                return Ok(None);
            }
        };

        mismatch = compute_mismatch(local, &remote);
        if mismatch <= threshold {
            warn!(
                attempt,
                mismatch = %mismatch,
                threshold = %threshold,
                "position-recon: mismatch cleared during confirmation"
            );
            return Ok(None);
        }

        warn!(
            attempt,
            mismatch = %mismatch,
            threshold = %threshold,
            "position-recon: mismatch persists during confirmation"
        );
    }

    Ok(Some(mismatch))
}

/// Compare local (risk engine tracked) vs remote positions.
/// Returns the total absolute mismatch in notional terms.
fn compute_mismatch(
    local: &HashMap<String, Decimal>,
    remote: &HashMap<String, Decimal>,
) -> Decimal {
    let mut total_mismatch = Decimal::ZERO;

    // Check all remote positions against local
    for (asset_id, remote_size) in remote {
        let local_size = local.get(asset_id).copied().unwrap_or(Decimal::ZERO);
        let diff = (remote_size - local_size).abs();
        total_mismatch += diff;
    }

    // Check local positions not in remote
    for (asset_id, local_size) in local {
        if !remote.contains_key(asset_id) {
            total_mismatch += local_size.abs();
        }
    }

    total_mismatch
}

/// Spawn the position reconciliation loop.
///
/// Polls the data API for positions every position_recon_interval,
/// compares against risk engine's tracked inventory, and halts on mismatch.
pub fn spawn_recon(
    config: &Config,
    _client: AuthClient,
    signer: Arc<Signer>,
    risk_engine: Arc<RiskEngine>,
) -> tokio::task::JoinHandle<()> {
    let interval = config.position_recon_interval;
    let nav = config.nav_usdc;
    let threshold =
        Decimal::from_f64_retain(nav * RECON_MISMATCH_THRESHOLD_PCT).unwrap_or(Decimal::from(100));
    let address = {
        use polymarket_client_sdk::derive_proxy_wallet;
        derive_proxy_wallet(signer.address(), config.chain_id)
            .map(|a| format!("{:#x}", a))
            .unwrap_or_else(|| format!("{:#x}", signer.address()))
    };
    let paper_mode = config.paper_mode;
    let data_host = data_host();
    let http_client = reqwest::Client::new();

    tokio::spawn(async move {
        let mut ticker = time::interval(interval);
        loop {
            ticker.tick().await;

            if risk_engine.is_halted() {
                debug!("position-recon: risk engine halted, skipping");
                continue;
            }

            // Paper fills never hit the chain — nothing to reconcile.
            if paper_mode {
                debug!("position-recon: paper mode, skipping");
                continue;
            }

            // Grace period after live fills: the data API lags 30-60s behind
            // on-chain settlement, so skip recon to avoid false mismatch halts.
            const FILL_GRACE_SECS: u64 = 90;
            let secs = risk_engine.secs_since_last_fill();
            if secs < FILL_GRACE_SECS {
                debug!(
                    secs_since_fill = secs,
                    grace = FILL_GRACE_SECS,
                    "position-recon: within fill grace period, skipping"
                );
                continue;
            }

            match fetch_remote_positions_from_host(&http_client, &data_host, &address).await {
                Ok(remote) => {
                    let local = risk_engine.inventory_snapshot();

                    let mismatch = compute_mismatch(&local, &remote);
                    if mismatch > threshold {
                        match confirm_persistent_mismatch(
                            &http_client,
                            &data_host,
                            &address,
                            &local,
                            threshold,
                            mismatch,
                        )
                        .await
                        {
                            Ok(Some(confirmed_mismatch)) => {
                                error!(
                                    mismatch = %confirmed_mismatch,
                                    threshold = %threshold,
                                    "position-recon: confirmed mismatch — halting"
                                );
                                risk_engine.halt("position reconciliation mismatch");
                            }
                            Ok(None) => {
                                warn!(
                                    mismatch = %mismatch,
                                    threshold = %threshold,
                                    "position-recon: mismatch not confirmed; continuing"
                                );
                            }
                            Err(e) => {
                                warn!(
                                    err = %e,
                                    "position-recon: confirmation failed; skipping halt"
                                );
                            }
                        }
                    } else {
                        debug!(
                            remote_positions = remote.len(),
                            mismatch = %mismatch,
                            "position-recon: OK"
                        );
                    }
                }
                Err(e) => {
                    // API failure — log but don't halt (transient errors expected)
                    warn!(err = %e, "position-recon: skipping tick due to API error");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn empty_snapshot() {
        let snap = PositionSnapshot::default();
        assert!(snap.positions.is_empty());
        assert_eq!(snap.unrealized_pnl, Decimal::ZERO);
    }

    #[test]
    fn snapshot_with_positions() {
        let mut snap = PositionSnapshot::default();
        snap.positions.insert("cond1".into(), Decimal::from(100));
        snap.positions.insert("cond2".into(), Decimal::from(-50));
        snap.unrealized_pnl = Decimal::from(25);

        assert_eq!(snap.positions.len(), 2);
        assert_eq!(snap.positions["cond1"], Decimal::from(100));
    }

    #[test]
    fn mismatch_empty_both() {
        let local = HashMap::new();
        let remote = HashMap::new();
        assert_eq!(compute_mismatch(&local, &remote), Decimal::ZERO);
    }

    #[test]
    fn mismatch_remote_only() {
        let local = HashMap::new();
        let mut remote = HashMap::new();
        remote.insert("t1".into(), Decimal::from(100));
        assert_eq!(compute_mismatch(&local, &remote), Decimal::from(100));
    }

    #[test]
    fn mismatch_local_only() {
        let mut local = HashMap::new();
        local.insert("t1".into(), Decimal::from(50));
        let remote = HashMap::new();
        assert_eq!(compute_mismatch(&local, &remote), Decimal::from(50));
    }

    #[test]
    fn mismatch_partial_overlap() {
        let mut local = HashMap::new();
        local.insert("t1".into(), Decimal::from(100));
        local.insert("t2".into(), Decimal::from(50));

        let mut remote = HashMap::new();
        remote.insert("t1".into(), Decimal::from(90)); // 10 diff
        remote.insert("t3".into(), Decimal::from(30)); // 30 diff (not in local)

        // t1: |90 - 100| = 10
        // t3: |30 - 0| = 30
        // t2: |50| = 50 (local only)
        // Total: 90
        assert_eq!(compute_mismatch(&local, &remote), Decimal::from(90));
    }

    #[test]
    fn mismatch_exact_match() {
        let mut local = HashMap::new();
        local.insert("t1".into(), Decimal::from(100));

        let mut remote = HashMap::new();
        remote.insert("t1".into(), Decimal::from(100));

        assert_eq!(compute_mismatch(&local, &remote), Decimal::ZERO);
    }

    #[tokio::test]
    async fn malformed_json_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/positions"))
            .and(query_param("user", "0xabc"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("{not-json", "application/json"))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let result = fetch_remote_positions_full_from_host(&client, &server.uri(), "0xabc").await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn retries_transient_position_api_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/positions"))
            .and(query_param("user", "0xabc"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/positions"))
            .and(query_param("user", "0xabc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "asset": "t1",
                    "conditionId": "cond1",
                    "size": 3.0
                }
            ])))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let result = fetch_remote_positions_full_from_host(&client, &server.uri(), "0xabc")
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].token_id, "t1");
        assert_eq!(result[0].size, Decimal::from(3));
    }

    #[tokio::test]
    async fn mismatch_requires_confirmation_before_halt() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/positions"))
            .and(query_param("user", "0xabc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "asset": "t1",
                    "conditionId": "cond1",
                    "size": 0.0
                }
            ])))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/positions"))
            .and(query_param("user", "0xabc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "asset": "t1",
                    "conditionId": "cond1",
                    "size": 10.0
                }
            ])))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let mut local = HashMap::new();
        local.insert("t1".into(), Decimal::from(10));

        let result = confirm_persistent_mismatch(
            &client,
            &server.uri(),
            "0xabc",
            &local,
            Decimal::from(5),
            Decimal::from(10),
        )
        .await
        .unwrap();

        assert!(result.is_none());
    }
}
