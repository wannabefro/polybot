use std::time::Duration;

use serde::Deserialize;

/// All tunable bot parameters, loaded from environment variables.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    // ── Connectivity ──
    pub clob_host: String,
    pub gamma_host: String,
    pub chain_id: u64,

    // ── Auth ──
    pub private_key: String,

    // ── Mode ──
    pub paper_mode: bool,

    // ── Risk limits (fractions of NAV) ──
    pub nav_usdc: f64,
    pub max_notional_per_market: f64,
    pub max_gross_exposure: f64,
    pub max_one_sided_inventory: f64,
    pub daily_loss_stop: f64,

    // ── Timing ──
    pub heartbeat_interval: Duration,
    pub geoblock_poll_interval: Duration,
    pub discovery_interval: Duration,
    pub position_recon_interval: Duration,
    pub stale_feed_threshold: Duration,

    // ── Mean-reversion strategy ──
    pub mean_revert_max_nav_frac: f64,
    pub mean_revert_min_volume_24h: f64,

    // ── Hedge SLA ──
    pub hedge_timeout: Duration,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            clob_host: env_or("POLYBOT_CLOB_HOST", "https://clob.polymarket.com"),
            gamma_host: env_or("POLYBOT_GAMMA_HOST", "https://gamma-api.polymarket.com"),
            chain_id: env_or("POLYBOT_CHAIN_ID", "137").parse()?,
            private_key: std::env::var("POLYBOT_PRIVATE_KEY")
                .map_err(|_| anyhow::anyhow!("POLYBOT_PRIVATE_KEY is required"))?,
            paper_mode: env_or("POLYBOT_PAPER_MODE", "true").parse()?,
            nav_usdc: env_or("POLYBOT_NAV_USDC", "1000.0").parse()?,
            max_notional_per_market: env_or("POLYBOT_MAX_NOTIONAL_PCT", "0.02").parse()?,
            max_gross_exposure: env_or("POLYBOT_MAX_GROSS_PCT", "0.25").parse()?,
            max_one_sided_inventory: env_or("POLYBOT_MAX_INVENTORY_PCT", "0.01").parse()?,
            daily_loss_stop: env_or("POLYBOT_DAILY_LOSS_PCT", "0.03").parse()?,
            heartbeat_interval: Duration::from_secs(
                env_or("POLYBOT_HEARTBEAT_SECS", "5").parse()?,
            ),
            geoblock_poll_interval: Duration::from_secs(
                env_or("POLYBOT_GEOBLOCK_POLL_SECS", "900").parse()?,
            ),
            discovery_interval: Duration::from_secs(
                env_or("POLYBOT_DISCOVERY_SECS", "60").parse()?,
            ),
            position_recon_interval: Duration::from_secs(
                env_or("POLYBOT_RECON_SECS", "45").parse()?,
            ),
            stale_feed_threshold: Duration::from_millis(
                env_or("POLYBOT_STALE_FEED_MS", "1500").parse()?,
            ),
            mean_revert_max_nav_frac: env_or("POLYBOT_MR_MAX_NAV_PCT", "0.005").parse()?,
            mean_revert_min_volume_24h: env_or("POLYBOT_MR_MIN_VOL_24H", "10000.0").parse()?,
            hedge_timeout: Duration::from_millis(
                env_or("POLYBOT_HEDGE_TIMEOUT_MS", "500").parse()?,
            ),
        })
    }

    /// Absolute USDC limit for a given NAV fraction.
    pub fn nav_limit(&self, fraction: f64) -> f64 {
        self.nav_usdc * fraction
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nav_limit_calculation() {
        let cfg = Config {
            clob_host: String::new(),
            gamma_host: String::new(),
            chain_id: 137,
            private_key: "deadbeef".into(),
            paper_mode: true,
            nav_usdc: 10_000.0,
            max_notional_per_market: 0.02,
            max_gross_exposure: 0.25,
            max_one_sided_inventory: 0.01,
            daily_loss_stop: 0.03,
            heartbeat_interval: Duration::from_secs(5),
            geoblock_poll_interval: Duration::from_secs(900),
            discovery_interval: Duration::from_secs(60),
            position_recon_interval: Duration::from_secs(45),
            stale_feed_threshold: Duration::from_millis(1500),
            mean_revert_max_nav_frac: 0.005,
            mean_revert_min_volume_24h: 10_000.0,
            hedge_timeout: Duration::from_millis(500),
        };
        assert!((cfg.nav_limit(0.02) - 200.0).abs() < f64::EPSILON);
        assert!((cfg.nav_limit(0.25) - 2500.0).abs() < f64::EPSILON);
    }
}
