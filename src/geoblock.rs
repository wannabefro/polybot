use anyhow::{bail, Result};
use tracing::info;

const GEOBLOCK_URL: &str = "https://polymarket.com/api/geoblock";

pub async fn check_or_abort() -> Result<()> {
    let resp = reqwest::get(GEOBLOCK_URL).await?;
    let body: serde_json::Value = resp.json().await?;

    if body.get("blocked").and_then(|v| v.as_bool()).unwrap_or(true) {
        bail!("geoblock: access blocked — aborting startup");
    }

    info!("geoblock: access permitted");
    Ok(())
}
