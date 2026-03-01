use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use rust_decimal::Decimal;
use tokio::time;
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
const RECON_MISMATCH_THRESHOLD_PCT: f64 = 0.01; // 1% NAV

/// Fetch positions from the Polymarket data API.
async fn fetch_remote_positions(address: &str) -> Result<HashMap<String, Decimal>> {
    let data_host = std::env::var("POLYBOT_DATA_HOST")
        .unwrap_or_else(|_| "https://data-api.polymarket.com".into());

    let url = format!("{}/positions?user={}", data_host, address);
    let resp = reqwest::get(&url).await;

    match resp {
        Ok(r) if r.status().is_success() => {
            #[derive(serde::Deserialize)]
            struct PosEntry {
                asset: Option<String>,
                size: Option<String>,
            }

            let entries: Vec<PosEntry> = r.json().await.unwrap_or_default();
            let mut map = HashMap::new();
            for entry in entries {
                if let (Some(asset), Some(size_str)) = (entry.asset, entry.size) {
                    if let Ok(size) = size_str.parse::<Decimal>() {
                        map.insert(asset, size);
                    }
                }
            }
            Ok(map)
        }
        Ok(r) => {
            warn!(status = %r.status(), "position-recon: data API returned error");
            Err(anyhow::anyhow!("data API error: {}", r.status()))
        }
        Err(e) => {
            warn!(err = %e, "position-recon: failed to reach data API");
            Err(e.into())
        }
    }
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
    let threshold = Decimal::from_f64_retain(nav * RECON_MISMATCH_THRESHOLD_PCT)
        .unwrap_or(Decimal::from(100));
    let address = format!("{:#x}", signer.address());

    tokio::spawn(async move {
        let mut ticker = time::interval(interval);
        loop {
            ticker.tick().await;

            if risk_engine.is_halted() {
                debug!("position-recon: risk engine halted, skipping");
                continue;
            }

            match fetch_remote_positions(&address).await {
                Ok(remote) => {
                    let local = risk_engine.inventory_snapshot();

                    let mismatch = compute_mismatch(&local, &remote);
                    if mismatch > threshold {
                        error!(
                            mismatch = %mismatch,
                            threshold = %threshold,
                            "position-recon: MISMATCH — halting"
                        );
                        risk_engine.halt("position reconciliation mismatch");
                    } else {
                        debug!(
                            remote_positions = remote.len(),
                            mismatch = %mismatch,
                            "position-recon: OK"
                        );
                    }
                }
                Err(_) => {
                    // API failure — log but don't halt (transient errors expected)
                    warn!("position-recon: skipping tick due to API error");
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
