use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Private key for L1 signing (hex, no 0x prefix)
    pub private_key: String,
    /// Polymarket CLOB API base URL
    pub clob_api_url: String,
    /// Polymarket Gamma API base URL
    pub gamma_api_url: String,
    /// Chain ID (137 = Polygon mainnet)
    pub chain_id: u64,
    /// Paper trading mode — no live orders
    pub paper_mode: bool,
    /// NAV in USDC (used for risk limit calculations)
    pub nav_usdc: f64,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            private_key: std::env::var("POLYBOT_PRIVATE_KEY")?,
            clob_api_url: std::env::var("POLYBOT_CLOB_URL")
                .unwrap_or_else(|_| "https://clob.polymarket.com".into()),
            gamma_api_url: std::env::var("POLYBOT_GAMMA_URL")
                .unwrap_or_else(|_| "https://gamma-api.polymarket.com".into()),
            chain_id: std::env::var("POLYBOT_CHAIN_ID")
                .unwrap_or_else(|_| "137".into())
                .parse()?,
            paper_mode: std::env::var("POLYBOT_PAPER_MODE")
                .unwrap_or_else(|_| "true".into())
                .parse()?,
            nav_usdc: std::env::var("POLYBOT_NAV_USDC")
                .unwrap_or_else(|_| "1000.0".into())
                .parse()?,
        })
    }
}
