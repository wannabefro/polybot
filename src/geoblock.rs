use anyhow::{bail, Result};
use tokio::time;
use tracing::{error, info};

use crate::config::Config;

const GEOBLOCK_URL: &str = "https://polymarket.com/api/geoblock";

/// Check geoblock once; bail if blocked.
async fn check() -> Result<bool> {
    let resp = reqwest::get(GEOBLOCK_URL).await?;
    let body: serde_json::Value = resp.json().await?;
    let blocked = body.get("blocked").and_then(|v| v.as_bool()).unwrap_or(true);
    Ok(blocked)
}

/// Startup gate — aborts the process if blocked.
pub async fn check_or_abort() -> Result<()> {
    if check().await? {
        bail!("geoblock: access blocked — aborting startup");
    }
    info!("geoblock: access permitted");
    Ok(())
}

/// Periodic geoblock monitor. Returns a task handle that sends `true` on the
/// channel if we become blocked (caller should cancel-all and halt).
pub fn spawn_monitor(
    config: &Config,
) -> (tokio::task::JoinHandle<()>, tokio::sync::watch::Receiver<bool>) {
    let interval = config.geoblock_poll_interval;
    let (tx, rx) = tokio::sync::watch::channel(false);

    let handle = tokio::spawn(async move {
        let mut ticker = time::interval(interval);
        ticker.tick().await; // skip immediate tick (startup check already ran)
        loop {
            ticker.tick().await;
            match check().await {
                Ok(true) => {
                    error!("geoblock: now BLOCKED — signaling halt");
                    let _ = tx.send(true);
                    return;
                }
                Ok(false) => {
                    info!("geoblock: periodic check passed");
                }
                Err(e) => {
                    error!("geoblock: check failed: {e} — treating as blocked");
                    let _ = tx.send(true);
                    return;
                }
            }
        }
    });

    (handle, rx)
}
