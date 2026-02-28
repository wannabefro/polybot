use std::sync::Arc;

use anyhow::Result;
use polymarket_client_sdk::clob::types::{OrderType, Side};
use polymarket_client_sdk::types::U256;
use rust_decimal::Decimal;
use tracing::{info, warn};

use crate::auth::{AuthClient, AuthContext};

/// Intent to place an order (pre-signing).
#[derive(Debug, Clone)]
pub struct OrderIntent {
    pub token_id: String,
    pub side: Side,
    pub price: Decimal,
    pub size: Decimal,
    pub order_type: OrderType,
    pub post_only: bool,
    /// Whether this order is for a neg-risk market.
    pub neg_risk: bool,
    /// Fee rate in basis points (from market metadata).
    pub fee_rate_bps: Decimal,
}

/// Result of submitting an order.
#[derive(Debug, Clone)]
pub struct OrderResult {
    pub order_id: String,
    pub intent: OrderIntent,
}

/// Place a single maker order (GTC + postOnly).
pub async fn place_maker_order(ctx: &AuthContext, intent: &OrderIntent) -> Result<OrderResult> {
    let token_id: U256 = intent
        .token_id
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid token_id"))?;

    let client = &*ctx.client;

    // Pre-populate SDK cache with neg_risk and fee_rate_bps from discovery metadata
    // so the order builder uses correct values without extra API calls.
    client.set_neg_risk(token_id, intent.neg_risk);
    let fee_bps_u32 = u32::try_from(intent.fee_rate_bps.mantissa())
        .unwrap_or(0);
    client.set_fee_rate_bps(token_id, fee_bps_u32);

    let signable = client
        .limit_order()
        .token_id(token_id)
        .side(intent.side)
        .price(intent.price)
        .size(intent.size)
        .order_type(intent.order_type.clone())
        .post_only(intent.post_only)
        .build()
        .await?;

    let signed = client.sign(&*ctx.signer, signable).await?;
    let resp = client.post_order(signed).await?;

    info!(
        order_id = %resp.order_id,
        side = ?intent.side,
        price = %intent.price,
        size = %intent.size,
        neg_risk = intent.neg_risk,
        "order: placed"
    );

    Ok(OrderResult {
        order_id: resp.order_id,
        intent: intent.clone(),
    })
}

/// Cancel a specific order.
pub async fn cancel(client: &AuthClient, order_id: &str) -> Result<()> {
    client.cancel_order(order_id).await?;
    info!(order_id, "order: cancelled");
    Ok(())
}

/// Emergency cancel-all.
pub async fn cancel_all(client: &AuthClient) -> Result<()> {
    let _resp = client.cancel_all_orders().await?;
    warn!("order: cancel-all executed");
    Ok(())
}
