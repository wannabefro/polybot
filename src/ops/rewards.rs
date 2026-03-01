use parking_lot::RwLock;
use rust_decimal::Decimal;
use std::sync::Arc;
use tracing::debug;

use crate::auth::AuthClient;

/// Tracks cumulative reward earnings from the Polymarket rewards program.
#[derive(Debug)]
pub struct RewardTracker {
    /// Total USDC earned from rewards (across all markets).
    pub total_earnings: RwLock<Decimal>,
    /// Number of markets actively earning rewards.
    pub earning_markets: RwLock<u32>,
}

impl RewardTracker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            total_earnings: RwLock::new(Decimal::ZERO),
            earning_markets: RwLock::new(0),
        })
    }

    /// Snapshot current earnings for display.
    pub fn total(&self) -> Decimal {
        *self.total_earnings.read()
    }

    #[allow(dead_code)]
    pub fn markets(&self) -> u32 {
        *self.earning_markets.read()
    }
}

/// Poll the CLOB API for current reward earnings and update the tracker.
/// Non-critical — failures are logged at debug level and silently ignored.
pub async fn refresh_earnings(client: &AuthClient, tracker: &RewardTracker) {
    use chrono::Utc;
    use polymarket_client_sdk::clob::types::request::UserRewardsEarningRequest;

    let today = Utc::now().date_naive();
    let request = UserRewardsEarningRequest::builder()
        .date(today)
        .build();

    match client.user_earnings_and_markets_config(&request, None).await {
        Ok(responses) => {
            let mut total = Decimal::ZERO;
            let mut count = 0u32;
            for market in &responses {
                for earning in &market.earnings {
                    total += earning.earnings;
                }
                if market.earning_percentage > Decimal::ZERO {
                    count += 1;
                }
            }
            *tracker.total_earnings.write() = total;
            *tracker.earning_markets.write() = count;
            debug!(total = %total, markets = count, "rewards: earnings refreshed");
        }
        Err(e) => {
            // SDK response struct may not match current API — non-critical
            debug!(err = %e, "rewards: earnings fetch failed (SDK compat)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn tracker_starts_at_zero() {
        let t = RewardTracker::new();
        assert_eq!(t.total(), Decimal::ZERO);
        assert_eq!(t.markets(), 0);
    }

    #[test]
    fn tracker_updates() {
        let t = RewardTracker::new();
        *t.total_earnings.write() = dec!(12.50);
        *t.earning_markets.write() = 3;
        assert_eq!(t.total(), dec!(12.50));
        assert_eq!(t.markets(), 3);
    }
}
