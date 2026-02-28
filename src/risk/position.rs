use std::sync::Arc;

use anyhow::Result;
use tokio::sync::watch;
use tokio::time;
use tracing::{error, info, warn};

use crate::auth::AuthClient;
use crate::config::Config;
use crate::risk::guardrails::RiskEngine;

/// Spawn the position reconciliation loop.
///
/// Polls GET /positions every position_recon_interval, compares with the
/// risk engine's local state, and halts on critical mismatches.
pub fn spawn_recon(
    config: &Config,
    client: AuthClient,
    risk_engine: Arc<RiskEngine>,
) -> tokio::task::JoinHandle<()> {
    let interval = config.position_recon_interval;

    tokio::spawn(async move {
        let mut ticker = time::interval(interval);
        loop {
            ticker.tick().await;

            // TODO: call client.positions() once we wire up the SDK method
            // For now, log that recon is running
            info!("position-recon: tick (SDK positions endpoint TBD)");
        }
    })
}
