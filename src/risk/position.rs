use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use rust_decimal::Decimal;
use tokio::time;
use tracing::{error, info, warn};

use crate::auth::AuthClient;
use crate::config::Config;
use crate::risk::guardrails::RiskEngine;

/// Snapshot of reconciled positions.
#[derive(Debug, Clone, Default)]
pub struct PositionSnapshot {
    /// condition_id → net size
    pub positions: HashMap<String, Decimal>,
    /// Total unrealized P&L across all positions.
    pub unrealized_pnl: Decimal,
}

/// Spawn the position reconciliation loop.
///
/// Polls the data API for positions every position_recon_interval,
/// updates the risk engine, and flags mismatches.
pub fn spawn_recon(
    config: &Config,
    _client: AuthClient,
    risk_engine: Arc<RiskEngine>,
) -> tokio::task::JoinHandle<()> {
    let interval = config.position_recon_interval;

    tokio::spawn(async move {
        let mut ticker = time::interval(interval);
        loop {
            ticker.tick().await;

            // TODO: Replace with actual SDK data::Client::positions() call
            // once the data client is wired into AuthContext.
            //
            // let positions = data_client.positions(&PositionsRequest::default()).await;
            // For now, just verify the risk engine is healthy.

            if risk_engine.is_halted() {
                warn!("position-recon: risk engine halted, skipping");
                continue;
            }

            info!("position-recon: tick — awaiting data client integration");
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
}
