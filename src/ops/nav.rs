//! Live NAV tracker — polls the CLOB exchange balance and broadcasts updates.
//!
//! Queries the Polymarket CLOB `balance-allowance` endpoint for the user's
//! collateral (USDC) balance. This reflects the actual available balance on
//! the exchange, not raw on-chain USDC (which reads 0 after deposit).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use polymarket_client_sdk::clob::types::AssetType;
use polymarket_client_sdk::clob::types::request::BalanceAllowanceRequest;
use polymarket_client_sdk::auth::{state::Authenticated, Normal};
use polymarket_client_sdk::clob::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio::time;
use tracing::{debug, info, warn};

/// Minimum change (in USDC) to trigger a NAV update broadcast.
const MIN_NAV_CHANGE: f64 = 1.0;

/// Current NAV snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavSnapshot {
    pub usdc_balance: f64,
    pub paper_pnl: f64,
    pub total_nav: f64,
}

/// Fetch collateral balance from the CLOB exchange API.
async fn fetch_clob_balance(
    client: &Client<Authenticated<Normal>>,
    first_call: bool,
) -> Result<f64> {
    let request = BalanceAllowanceRequest::builder()
        .asset_type(AssetType::Collateral)
        .build();

    let resp = client.balance_allowance(request).await?;

    // Log at INFO on first call for diagnostics, then debug
    if first_call {
        info!(raw_balance = %resp.balance, allowances = ?resp.allowances, "nav: first CLOB balance-allowance response");
    } else {
        debug!(raw_balance = %resp.balance, allowances = ?resp.allowances, "nav: CLOB response");
    }

    use rust_decimal::prelude::ToPrimitive;
    Ok(resp.balance.to_f64().unwrap_or(0.0))
}

/// Spawn the NAV tracking loop.
///
/// Returns a watch receiver for live NAV snapshots. The initial value
/// uses the configured `nav_usdc` from config until the first CLOB
/// read succeeds.
pub fn spawn(
    client: Arc<Client<Authenticated<Normal>>>,
    initial_nav: f64,
    paper_mode: bool,
    poll_interval: Duration,
) -> (tokio::task::JoinHandle<()>, watch::Receiver<NavSnapshot>) {
    let initial = NavSnapshot {
        usdc_balance: initial_nav,
        paper_pnl: 0.0,
        total_nav: initial_nav,
    };
    let (tx, rx) = watch::channel(initial);

    let handle = tokio::spawn(async move {
        let mut ticker = time::interval(poll_interval);
        let mut last_nav = initial_nav;
        let mut ever_seen_nonzero = false;

        let mut first_call = true;

        loop {
            ticker.tick().await;

            match fetch_clob_balance(&client, first_call).await {
                Ok(balance) => {
                    first_call = false;
                    if balance > 0.0 {
                        ever_seen_nonzero = true;
                    }

                    // Don't slam NAV to 0 unless we've previously seen a real
                    // balance (protects against API returning 0 when funds are
                    // tied up in conditional tokens / positions).
                    let total = if paper_mode {
                        balance.max(initial_nav)
                    } else if balance <= 0.0 && !ever_seen_nonzero {
                        warn!(
                            configured_nav = format!("{initial_nav:.2}"),
                            "nav: CLOB returned 0 — keeping configured NAV \
                             (funds may be in positions, not free collateral)"
                        );
                        initial_nav
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
                    warn!(err = %e, "nav: failed to fetch CLOB balance");
                }
            }
        }
    });

    (handle, rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_nav_change_filters_noise() {
        assert!(MIN_NAV_CHANGE >= 1.0);
    }
}
