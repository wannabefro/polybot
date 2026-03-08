use anyhow::{bail, Result};
use tokio::time;
use tracing::{error, info};

use crate::config::Config;

const GEOBLOCK_URL: &str = "https://polymarket.com/api/geoblock";

/// Check geoblock once; bail if blocked.
/// Uses curl as fallback when reqwest fails (some networks block Rust TLS fingerprints).
async fn check() -> Result<bool> {
    // Try reqwest first
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(8))
        .build()?;
    match client.get(GEOBLOCK_URL).send().await {
        Ok(resp) => {
            let body: serde_json::Value = resp.json().await?;
            let blocked = body
                .get("blocked")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            return Ok(blocked);
        }
        Err(_) => {
            // Fallback to curl (different TLS stack, avoids fingerprint blocking)
            let output = tokio::process::Command::new("curl")
                .args(["-s", "--max-time", "10", GEOBLOCK_URL])
                .output()
                .await?;
            if !output.status.success() {
                bail!("geoblock: curl fallback also failed");
            }
            let body: serde_json::Value = serde_json::from_slice(&output.stdout)?;
            let blocked = body
                .get("blocked")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            Ok(blocked)
        }
    }
}

/// Startup gate — retries up to 3 times, aborts if blocked.
pub async fn check_or_abort() -> Result<()> {
    for attempt in 1..=3 {
        match check().await {
            Ok(false) => {
                info!("geoblock: access permitted");
                return Ok(());
            }
            Ok(true) => {
                bail!("geoblock: access blocked — aborting startup");
            }
            Err(e) if attempt < 3 => {
                tracing::warn!(attempt, err = %e, "geoblock: connection failed, retrying in 2s");
                time::sleep(std::time::Duration::from_secs(2)).await;
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// Periodic geoblock monitor. Returns a task handle that sends `true` on the
/// channel if we become blocked (caller should cancel-all and halt).
pub fn spawn_monitor(
    config: &Config,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::watch::Receiver<bool>,
) {
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
                    tracing::warn!("geoblock: check failed: {e} — will retry next tick");
                }
            }
        }
    });

    (handle, rx)
}
