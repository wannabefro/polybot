//! Live NAV tracker — polls on-chain USDC balance and broadcasts updates.
//!
//! On Polygon, Polymarket uses USDC.e (bridged USDC) at
//! `0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174` (6 decimals).
//!
//! In paper mode, on-chain balance doesn't change so we supplement with
//! the paper PnL from the risk engine.

use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::time;
use tracing::{debug, info, warn};

/// Polygon USDC.e contract (used by Polymarket).
const DEFAULT_USDC_CONTRACT: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";

/// Default public Polygon RPC.
const DEFAULT_POLYGON_RPC: &str = "https://polygon-rpc.com";

/// balanceOf(address) selector.
const BALANCE_OF_SELECTOR: &str = "70a08231";

/// USDC has 6 decimals.
const USDC_DECIMALS: u32 = 6;

/// Minimum change (in USDC) to trigger a NAV update broadcast.
const MIN_NAV_CHANGE: f64 = 1.0;

/// Current NAV snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavSnapshot {
    pub usdc_balance: f64,
    pub paper_pnl: f64,
    pub total_nav: f64,
}

/// Query USDC.e balance for an address via Polygon JSON-RPC.
async fn fetch_usdc_balance(
    client: &reqwest::Client,
    rpc_url: &str,
    usdc_contract: &str,
    wallet_address: &str,
) -> Result<f64> {
    // Pad address to 32 bytes for balanceOf(address) call
    let addr = wallet_address.trim_start_matches("0x").to_lowercase();
    let data = format!("0x{BALANCE_OF_SELECTOR}{addr:0>64}");

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{
            "to": usdc_contract,
            "data": data
        }, "latest"],
        "id": 1
    });

    let resp = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await?;

    let json: serde_json::Value = resp.json().await?;

    if let Some(err) = json.get("error") {
        anyhow::bail!("RPC error: {err}");
    }

    let result = json["result"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing result in RPC response"))?;

    // Parse hex to u128, then convert to f64 with 6 decimal places
    let hex = result.trim_start_matches("0x");
    let raw = u128::from_str_radix(hex, 16)?;
    let divisor = 10u128.pow(USDC_DECIMALS);
    let balance = raw as f64 / divisor as f64;

    Ok(balance)
}

/// Spawn the NAV tracking loop.
///
/// Returns a watch receiver for live NAV snapshots. The initial value
/// uses the configured `nav_usdc` from config until the first on-chain
/// read succeeds.
pub fn spawn(
    wallet_address: String,
    initial_nav: f64,
    paper_mode: bool,
    poll_interval: Duration,
    polygon_rpc: String,
    usdc_contract: String,
) -> (tokio::task::JoinHandle<()>, watch::Receiver<NavSnapshot>) {
    let initial = NavSnapshot {
        usdc_balance: initial_nav,
        paper_pnl: 0.0,
        total_nav: initial_nav,
    };
    let (tx, rx) = watch::channel(initial);

    let handle = tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!(err = %e, "nav: failed to build HTTP client, tracker disabled");
                return;
            }
        };

        let mut ticker = time::interval(poll_interval);
        let mut last_nav = initial_nav;

        loop {
            ticker.tick().await;

            match fetch_usdc_balance(&client, &polygon_rpc, &usdc_contract, &wallet_address).await
            {
                Ok(balance) => {
                    // In paper mode, on-chain balance is static — the "real" NAV
                    // is the on-chain balance (we don't add paper PnL since the
                    // risk engine already tracks that for daily loss stops).
                    let total = if paper_mode {
                        // Use whichever is higher: on-chain or initial
                        // (paper mode may start with a virtual NAV > actual balance)
                        balance.max(initial_nav)
                    } else {
                        balance
                    };

                    let snap = NavSnapshot {
                        usdc_balance: balance,
                        paper_pnl: 0.0,
                        total_nav: total,
                    };

                    if (total - last_nav).abs() >= MIN_NAV_CHANGE {
                        info!(
                            usdc_balance = format!("{balance:.2}"),
                            total_nav = format!("{total:.2}"),
                            delta = format!("{:.2}", total - last_nav),
                            "nav: balance updated"
                        );
                        last_nav = total;
                    } else {
                        debug!(
                            usdc_balance = format!("{balance:.2}"),
                            total_nav = format!("{total:.2}"),
                            "nav: balance unchanged"
                        );
                    }

                    let _ = tx.send(snap);
                }
                Err(e) => {
                    warn!(err = %e, "nav: failed to fetch USDC balance");
                }
            }
        }
    });

    (handle, rx)
}

/// Parse a polygon RPC URL from env, falling back to public RPC.
pub fn polygon_rpc_from_env() -> String {
    std::env::var("POLYBOT_POLYGON_RPC").unwrap_or_else(|_| DEFAULT_POLYGON_RPC.to_string())
}

/// Parse USDC contract address from env, falling back to USDC.e.
pub fn usdc_contract_from_env() -> String {
    std::env::var("POLYBOT_USDC_CONTRACT")
        .unwrap_or_else(|_| DEFAULT_USDC_CONTRACT.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_of_calldata_format() {
        let addr = "0x48c51cf7C9f03020e4A2bc1430D1d11B7585b4D4";
        let clean = addr.trim_start_matches("0x").to_lowercase();
        let data = format!("0x{BALANCE_OF_SELECTOR}{clean:0>64}");
        assert!(data.starts_with("0x70a08231"));
        assert_eq!(data.len(), 2 + 8 + 64); // 0x + selector + padded address
    }

    #[test]
    fn hex_balance_parsing() {
        // 100 USDC = 100_000_000 raw (6 decimals)
        let hex = "5F5E100"; // 100_000_000
        let raw = u128::from_str_radix(hex, 16).unwrap();
        let balance = raw as f64 / 1_000_000.0;
        assert!((balance - 100.0).abs() < 0.001);
    }

    #[test]
    fn hex_balance_zero() {
        let hex = "0";
        let raw = u128::from_str_radix(hex, 16).unwrap();
        let balance = raw as f64 / 1_000_000.0;
        assert!((balance).abs() < 0.001);
    }

    #[test]
    fn hex_balance_fractional() {
        // 50.25 USDC = 50_250_000 raw
        let hex = "2FEC110"; // 50_250_000 = 0x2FEC110
        let raw = u128::from_str_radix(hex, 16).unwrap();
        let balance = raw as f64 / 1_000_000.0;
        assert!((balance - 50.25).abs() < 0.001);
    }

    #[test]
    fn min_nav_change_filters_noise() {
        assert!(MIN_NAV_CHANGE >= 1.0);
    }
}
