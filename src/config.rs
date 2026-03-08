use std::time::Duration;

use serde::Deserialize;

/// All tunable bot parameters, loaded from environment variables.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    // ── Connectivity ──
    pub clob_host: String,
    pub gamma_host: String,
    pub chain_id: u64,

    // ── SOCKS5 proxy (optional) ──
    pub socks5_proxy: Option<String>,

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

    // ── Operational tuning ──
    pub rate_limit_per_sec: f64,
    pub llm_poll_interval: Duration,
    pub metrics_interval: Duration,
    pub mm_min_size: f64,
    pub reward_min_size: f64,
    pub reward_min_nav_usdc: f64,
    pub mean_revert_min_nav_usdc: f64,
    pub small_account_nav_threshold: f64,
    pub small_account_min_per_market_pct: f64,
    pub small_account_min_inventory_pct: f64,
    pub neg_risk_stale_secs: u64,
    pub quote_tick_secs: u64,
    #[allow(dead_code)]
    pub quote_max_age: Duration,
    pub max_ws_tokens: usize,
    pub scalper_min_spread_ticks: u32,
    pub scalper_min_touch_notional_usdc: f64,
    pub scalper_min_vol_24h: f64,
    pub scalper_max_vol_24h: f64,
    pub scalper_refresh_secs: u64,
    pub scalper_reprice_ticks: u32,
    pub scalper_small_max_markets: usize,
    pub unwind_stage1_secs: u64,
    pub unwind_stage2_secs: u64,
    pub unwind_hard_stop_secs: u64,
    pub unwind_cooldown_secs: u64,

    // ── Decay (time-decay penny collector) strategy ──
    pub decay_enabled: bool,
    pub decay_min_price: f64,
    pub decay_sports_min_price: f64,
    pub decay_max_bet_usdc: f64,
    pub decay_sports_max_bet_usdc: f64,
    pub decay_window_hours: f64,
    pub decay_sports_window_hours: f64,
    pub decay_nav_fraction: f64,
    pub decay_excluded_tags: Vec<String>,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            clob_host: env_or("POLYBOT_CLOB_HOST", "https://clob.polymarket.com"),
            gamma_host: env_or("POLYBOT_GAMMA_HOST", "https://gamma-api.polymarket.com"),
            chain_id: parse_env("POLYBOT_CHAIN_ID", "137")?,
            private_key: std::env::var("POLYBOT_PRIVATE_KEY")
                .map_err(|_| anyhow::anyhow!("POLYBOT_PRIVATE_KEY is required"))?,
            paper_mode: parse_env("POLYBOT_PAPER_MODE", "true")?,
            nav_usdc: parse_env("POLYBOT_NAV_USDC", "1000.0")?,
            max_notional_per_market: parse_env("POLYBOT_MAX_NOTIONAL_PCT", "0.02")?,
            max_gross_exposure: parse_env("POLYBOT_MAX_GROSS_PCT", "0.50")?,
            max_one_sided_inventory: parse_env("POLYBOT_MAX_INVENTORY_PCT", "0.04")?,
            daily_loss_stop: parse_env("POLYBOT_DAILY_LOSS_PCT", "0.05")?,
            heartbeat_interval: Duration::from_secs(parse_env("POLYBOT_HEARTBEAT_SECS", "5")?),
            geoblock_poll_interval: Duration::from_secs(parse_env(
                "POLYBOT_GEOBLOCK_POLL_SECS",
                "900",
            )?),
            discovery_interval: Duration::from_secs(parse_env("POLYBOT_DISCOVERY_SECS", "60")?),
            position_recon_interval: Duration::from_secs(parse_env("POLYBOT_RECON_SECS", "45")?),
            stale_feed_threshold: Duration::from_millis(parse_env(
                "POLYBOT_STALE_FEED_MS",
                "1500",
            )?),
            mean_revert_max_nav_frac: parse_env("POLYBOT_MR_MAX_NAV_PCT", "0.005")?,
            mean_revert_min_volume_24h: parse_env("POLYBOT_MR_MIN_VOL_24H", "10000.0")?,
            hedge_timeout: Duration::from_secs(parse_env("POLYBOT_HEDGE_TIMEOUT_SECS", "120")?),
            rate_limit_per_sec: parse_env("POLYBOT_RATE_LIMIT_PS", "70.0")?,
            llm_poll_interval: Duration::from_secs(parse_env("POLYBOT_LLM_POLL_SECS", "10")?),
            metrics_interval: Duration::from_secs(parse_env("POLYBOT_METRICS_SECS", "60")?),
            mm_min_size: parse_env("POLYBOT_MM_MIN_SIZE", "1.0")?,
            reward_min_size: parse_env("POLYBOT_REWARD_MIN_SIZE", "1.0")?,
            reward_min_nav_usdc: parse_env("POLYBOT_REWARD_MIN_NAV_USDC", "25.0")?,
            mean_revert_min_nav_usdc: parse_env("POLYBOT_MR_MIN_NAV_USDC", "100.0")?,
            small_account_nav_threshold: parse_env("POLYBOT_SMALL_ACCOUNT_NAV_THRESHOLD", "500.0")?,
            small_account_min_per_market_pct: parse_env(
                "POLYBOT_SMALL_MIN_PER_MARKET_PCT",
                "0.12",
            )?,
            small_account_min_inventory_pct: parse_env("POLYBOT_SMALL_MIN_INVENTORY_PCT", "0.10")?,
            neg_risk_stale_secs: parse_env("POLYBOT_NR_STALE_SECS", "10")?,
            quote_tick_secs: parse_env("POLYBOT_QUOTE_TICK_SECS", "10")?,
            quote_max_age: Duration::from_secs(parse_env("POLYBOT_QUOTE_MAX_AGE_SECS", "60")?),
            max_ws_tokens: parse_env("POLYBOT_MAX_WS_TOKENS", "500")?,
            scalper_min_spread_ticks: parse_env("POLYBOT_SCALPER_MIN_SPREAD_TICKS", "4")?,
            scalper_min_touch_notional_usdc: parse_env(
                "POLYBOT_SCALPER_MIN_TOUCH_NOTIONAL_USDC",
                "10.0",
            )?,
            scalper_min_vol_24h: parse_env("POLYBOT_SCALPER_MIN_VOL_24H", "1000.0")?,
            scalper_max_vol_24h: parse_env("POLYBOT_SCALPER_MAX_VOL_24H", "250000.0")?,
            scalper_refresh_secs: parse_env("POLYBOT_SCALPER_REFRESH_SECS", "30")?,
            scalper_reprice_ticks: parse_env("POLYBOT_SCALPER_REPRICE_TICKS", "3")?,
            scalper_small_max_markets: parse_env("POLYBOT_SCALPER_SMALL_MAX_MARKETS", "2")?,
            unwind_stage1_secs: parse_env("POLYBOT_UNWIND_STAGE1_SECS", "60")?,
            unwind_stage2_secs: parse_env("POLYBOT_UNWIND_STAGE2_SECS", "90")?,
            unwind_hard_stop_secs: parse_env("POLYBOT_UNWIND_HARD_STOP_SECS", "120")?,
            unwind_cooldown_secs: parse_env("POLYBOT_UNWIND_COOLDOWN_SECS", "900")?,
            decay_enabled: parse_env("POLYBOT_DECAY_ENABLED", "true")?,
            decay_min_price: parse_env("POLYBOT_DECAY_MIN_PRICE", "0.85")?,
            decay_sports_min_price: parse_env("POLYBOT_DECAY_SPORTS_MIN_PRICE", "0.90")?,
            decay_max_bet_usdc: parse_env("POLYBOT_DECAY_MAX_BET_USDC", "15.0")?,
            decay_sports_max_bet_usdc: parse_env("POLYBOT_DECAY_SPORTS_MAX_BET_USDC", "5.0")?,
            decay_window_hours: parse_env("POLYBOT_DECAY_WINDOW_HOURS", "24.0")?,
            decay_sports_window_hours: parse_env("POLYBOT_DECAY_SPORTS_WINDOW_HOURS", "0.083")?,
            decay_nav_fraction: parse_env("POLYBOT_DECAY_NAV_FRACTION", "0.70")?,
            decay_excluded_tags: env_or(
                "POLYBOT_DECAY_EXCLUDED_TAGS",
                "crypto,bitcoin,ethereum,btc,eth",
            )
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
            socks5_proxy: {
                let host = std::env::var("NORDVPN_HOST").ok();
                let user = std::env::var("NORDVPN_USERNAME").ok();
                let pass = std::env::var("NORDVPN_PASSWORD").ok();
                match (host, user, pass) {
                    (Some(h), Some(u), Some(p)) => Some(format!("socks5://{u}:{p}@{h}:1080")),
                    _ => std::env::var("POLYBOT_SOCKS5_PROXY").ok(),
                }
            },
        })
    }

    /// Absolute USDC limit for a given NAV fraction.
    pub fn nav_limit(&self, fraction: f64) -> f64 {
        self.nav_usdc * fraction
    }

    /// True when account NAV is in the small-account bucket.
    pub fn is_small_account(&self) -> bool {
        self.nav_usdc <= self.small_account_nav_threshold
    }

    /// Effective per-market cap fraction used by the runtime.
    pub fn effective_max_notional_per_market(&self) -> f64 {
        if self.is_small_account() {
            self.max_notional_per_market
                .max(self.small_account_min_per_market_pct)
        } else {
            self.max_notional_per_market
        }
    }

    /// Effective one-sided inventory cap fraction used by the runtime.
    pub fn effective_max_one_sided_inventory(&self) -> f64 {
        if self.is_small_account() {
            self.max_one_sided_inventory
                .max(self.small_account_min_inventory_pct)
        } else {
            self.max_one_sided_inventory
        }
    }

    /// Effective gross exposure cap — raised for small accounts to allow more markets.
    pub fn effective_max_gross_exposure(&self) -> f64 {
        if self.is_small_account() {
            self.max_gross_exposure.max(0.40) // 40% floor for small accounts
        } else {
            self.max_gross_exposure
        }
    }

    /// Max reward markets to actively quote given NAV — avoids spreading too thin.
    pub fn max_active_markets(&self) -> usize {
        if self.nav_usdc < 100.0 {
            3
        } else if self.nav_usdc < 250.0 {
            5
        } else if self.nav_usdc < 1000.0 {
            15
        } else {
            usize::MAX // no limit
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

fn parse_env<T: std::str::FromStr>(key: &str, default: &str) -> anyhow::Result<T>
where
    T::Err: std::fmt::Display,
{
    let raw = env_or(key, default);
    raw.parse::<T>()
        .map_err(|e| anyhow::anyhow!("failed to parse {key}={raw:?}: {e}"))
}

/// Build a Config with safe defaults for testing (no env vars needed).
/// Available in both unit and integration tests.
#[doc(hidden)]
#[allow(dead_code)]
pub fn test_config() -> Config {
    Config {
        clob_host: "https://clob.polymarket.com".into(),
        gamma_host: "https://gamma-api.polymarket.com".into(),
        chain_id: 137,
        private_key: "deadbeef".into(),
        paper_mode: true,
        nav_usdc: 10_000.0,
        max_notional_per_market: 0.02,
        max_gross_exposure: 0.50,
        max_one_sided_inventory: 0.04,
        daily_loss_stop: 0.05,
        heartbeat_interval: Duration::from_secs(5),
        geoblock_poll_interval: Duration::from_secs(900),
        discovery_interval: Duration::from_secs(60),
        position_recon_interval: Duration::from_secs(45),
        stale_feed_threshold: Duration::from_millis(1500),
        mean_revert_max_nav_frac: 0.005,
        mean_revert_min_volume_24h: 10_000.0,
        hedge_timeout: Duration::from_secs(300),
        rate_limit_per_sec: 70.0,
        llm_poll_interval: Duration::from_secs(10),
        metrics_interval: Duration::from_secs(30),
        mm_min_size: 1.0,
        reward_min_size: 1.0,
        reward_min_nav_usdc: 25.0,
        mean_revert_min_nav_usdc: 100.0,
        small_account_nav_threshold: 500.0,
        small_account_min_per_market_pct: 0.12,
        small_account_min_inventory_pct: 0.10,
        neg_risk_stale_secs: 10,
        quote_tick_secs: 10,
        quote_max_age: Duration::from_secs(20),
        max_ws_tokens: 500,
        scalper_min_spread_ticks: 4,
        scalper_min_touch_notional_usdc: 10.0,
        scalper_min_vol_24h: 1000.0,
        scalper_max_vol_24h: 250_000.0,
        scalper_refresh_secs: 30,
        scalper_reprice_ticks: 3,
        scalper_small_max_markets: 2,
        unwind_stage1_secs: 60,
        unwind_stage2_secs: 90,
        unwind_hard_stop_secs: 120,
        unwind_cooldown_secs: 900,
        decay_enabled: true,
        decay_min_price: 0.85,
        decay_sports_min_price: 0.90,
        decay_max_bet_usdc: 15.0,
        decay_sports_max_bet_usdc: 5.0,
        decay_window_hours: 24.0,
        decay_sports_window_hours: 0.083,
        decay_nav_fraction: 0.70,
        decay_excluded_tags: vec![
            "crypto".into(),
            "bitcoin".into(),
            "ethereum".into(),
            "btc".into(),
            "eth".into(),
        ],
        socks5_proxy: None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // Re-export for crate-internal tests
    pub use super::test_config;

    #[test]
    fn nav_limit_calculation() {
        let cfg = test_config();
        assert!((cfg.nav_limit(0.02) - 200.0).abs() < f64::EPSILON);
        assert!((cfg.nav_limit(0.25) - 2500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn nav_limit_zero_fraction() {
        let cfg = test_config();
        assert!((cfg.nav_limit(0.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn nav_limit_full_nav() {
        let cfg = test_config();
        assert!((cfg.nav_limit(1.0) - 10_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn from_env_behavior() {
        // Test both missing-key error and successful parse sequentially
        // to avoid env var race conditions in parallel test runner.

        // Part 1: missing key → error
        std::env::remove_var("POLYBOT_PRIVATE_KEY");
        let result = Config::from_env();
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("POLYBOT_PRIVATE_KEY"),
            "error should mention the missing env var"
        );

        // Part 2: with key → success
        std::env::set_var("POLYBOT_PRIVATE_KEY", "0xdeadbeef");
        let result = Config::from_env();
        std::env::remove_var("POLYBOT_PRIVATE_KEY");
        let cfg = result.unwrap();
        assert_eq!(cfg.private_key, "0xdeadbeef");
        assert!(cfg.paper_mode);
        assert_eq!(cfg.chain_id, 137);
    }

    #[test]
    fn env_or_returns_default() {
        let val = env_or("POLYBOT_NONEXISTENT_VAR_12345", "fallback");
        assert_eq!(val, "fallback");
    }

    #[test]
    fn env_or_returns_env_value() {
        std::env::set_var("POLYBOT_TEST_VAR_99", "custom_value");
        let val = env_or("POLYBOT_TEST_VAR_99", "fallback");
        std::env::remove_var("POLYBOT_TEST_VAR_99");
        assert_eq!(val, "custom_value");
    }

    #[test]
    fn default_hosts_are_production() {
        let cfg = test_config();
        assert!(cfg.clob_host.contains("polymarket.com"));
        assert!(cfg.gamma_host.contains("polymarket.com"));
    }

    #[test]
    fn default_risk_limits_match_plan() {
        let cfg = test_config();
        assert_eq!(cfg.max_notional_per_market, 0.02); // 2%
        assert_eq!(cfg.max_gross_exposure, 0.50); // 50%
        assert_eq!(cfg.max_one_sided_inventory, 0.04); // 4%
        assert_eq!(cfg.daily_loss_stop, 0.05); // 5%
        assert_eq!(cfg.mean_revert_max_nav_frac, 0.005); // 0.5%
    }

    #[test]
    fn effective_caps_increase_for_small_accounts() {
        let mut cfg = test_config();
        cfg.nav_usdc = 100.0;
        assert!(cfg.is_small_account());
        assert_eq!(cfg.effective_max_notional_per_market(), 0.12);
        assert_eq!(cfg.effective_max_one_sided_inventory(), 0.10);
        assert_eq!(cfg.effective_max_gross_exposure(), 0.50);
        assert_eq!(cfg.max_active_markets(), 5);
    }

    #[test]
    fn effective_caps_unchanged_for_large_accounts() {
        let cfg = test_config();
        assert!(!cfg.is_small_account());
        assert_eq!(
            cfg.effective_max_notional_per_market(),
            cfg.max_notional_per_market
        );
        assert_eq!(
            cfg.effective_max_one_sided_inventory(),
            cfg.max_one_sided_inventory
        );
        assert_eq!(cfg.effective_max_gross_exposure(), cfg.max_gross_exposure);
        assert_eq!(cfg.max_active_markets(), usize::MAX);
    }

    #[test]
    fn max_active_markets_tiers() {
        let mut cfg = test_config();
        cfg.nav_usdc = 50.0;
        assert_eq!(cfg.max_active_markets(), 3);
        cfg.nav_usdc = 200.0;
        assert_eq!(cfg.max_active_markets(), 5);
        cfg.nav_usdc = 500.0;
        assert_eq!(cfg.max_active_markets(), 15);
        cfg.nav_usdc = 5000.0;
        assert_eq!(cfg.max_active_markets(), usize::MAX);
    }
}
