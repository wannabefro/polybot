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
