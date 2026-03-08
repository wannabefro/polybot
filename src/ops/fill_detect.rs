//! Live fill detection: polls CLOB for order status changes.
//!
//! Post-only GTC orders rest on the book. When they fill, the only way to
//! know is to poll the order status endpoint. This module checks active
//! orders and returns detected fills.

use polymarket_client_sdk::clob::types::OrderStatusType;
use rust_decimal::Decimal;
use tracing::{debug, warn};

use crate::auth::AuthClient;

/// A fill detected by polling order status.
#[derive(Debug, Clone)]
pub struct DetectedFill {
    pub order_id: String,
    pub size_matched: Decimal,
}

/// Poll the CLOB for order statuses and return any that have new fills.
/// `order_ids` should be the IDs of active resting orders.
/// Returns fills for orders where `size_matched > 0`.
pub async fn detect_fills(client: &AuthClient, order_ids: &[String]) -> Vec<DetectedFill> {
    let mut fills = Vec::new();

    for order_id in order_ids {
        match client.order(order_id).await {
            Ok(resp) => {
                if resp.size_matched > Decimal::ZERO {
                    debug!(
                        order_id = %order_id,
                        status = ?resp.status,
                        matched = %resp.size_matched,
                        original = %resp.original_size,
                        "fill_detect: order has matched size"
                    );
                    fills.push(DetectedFill {
                        order_id: order_id.clone(),
                        size_matched: resp.size_matched,
                    });
                }
                // If cancelled/expired, we'll detect it here too
                if matches!(resp.status, OrderStatusType::Canceled) {
                    debug!(order_id = %order_id, "fill_detect: order was cancelled externally");
                }
            }
            Err(e) => {
                // Don't spam on transient errors
                let msg = e.to_string();
                if msg.contains("404") || msg.contains("not found") {
                    debug!(order_id = %order_id, "fill_detect: order not found (likely expired)");
                } else {
                    warn!(order_id = %order_id, err = %e, "fill_detect: failed to poll order");
                }
            }
        }
    }

    fills
}
