
use anyhow::Result;
use polymarket_client_sdk::clob::types::{OrderType, Side};
use polymarket_client_sdk::types::U256;
use rust_decimal::Decimal;
use tracing::{debug, warn};

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
#[allow(dead_code)]
pub struct OrderResult {
    pub order_id: String,
    pub intent: OrderIntent,
    /// True if the CLOB matched this order (e.g. FOK fills).
    pub matched: bool,
    /// Size filled by the CLOB, from making_amount.
    pub making_amount: Decimal,
    /// USDC side of the match, from taking_amount.
    pub taking_amount: Decimal,
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
        .size(intent.size.trunc_with_scale(2)) // max 2 decimal places (lot size)
        .order_type(intent.order_type.clone())
        .post_only(intent.post_only)
        .build()
        .await?;

    let signed = client.sign(&*ctx.signer, signable).await?;
    let resp = client.post_order(signed).await?;

    debug!(
        order_id = %resp.order_id,
        side = ?intent.side,
        price = %intent.price,
        size = %intent.size.trunc_with_scale(2),
        "order: placed"
    );

    let matched = resp.status == polymarket_client_sdk::clob::types::OrderStatusType::Matched;
    Ok(OrderResult {
        order_id: resp.order_id,
        intent: intent.clone(),
        matched,
        making_amount: resp.making_amount,
        taking_amount: resp.taking_amount,
    })
}

/// Cancel specific orders in a batch. Returns the count cancelled.
pub async fn cancel_batch(client: &AuthClient, order_ids: &[String]) -> Result<usize> {
    if order_ids.is_empty() {
        return Ok(0);
    }
    let refs: Vec<&str> = order_ids.iter().map(|s| s.as_str()).collect();
    client.cancel_orders(&refs).await?;
    debug!(count = order_ids.len(), "order: batch cancelled");
    Ok(order_ids.len())
}

/// Place multiple orders in a single batch API call. Returns results per order.
#[allow(dead_code)]
pub async fn place_batch(
    ctx: &AuthContext,
    intents: &[OrderIntent],
) -> Vec<Result<OrderResult>> {
    if intents.is_empty() {
        return Vec::new();
    }

    let client = &*ctx.client;

    // Sign all orders
    let mut signed_orders = Vec::with_capacity(intents.len());
    let mut valid_indices = Vec::with_capacity(intents.len());
    let mut results: Vec<Option<Result<OrderResult>>> = (0..intents.len()).map(|_| None).collect();

    for (i, intent) in intents.iter().enumerate() {
        let token_id: U256 = match intent.token_id.parse() {
            Ok(id) => id,
            Err(_) => {
                results[i] = Some(Err(anyhow::anyhow!("invalid token_id")));
                continue;
            }
        };

        client.set_neg_risk(token_id, intent.neg_risk);
        let fee_bps_u32 = u32::try_from(intent.fee_rate_bps.mantissa()).unwrap_or(0);
        client.set_fee_rate_bps(token_id, fee_bps_u32);

        let signable = match client
            .limit_order()
            .token_id(token_id)
            .side(intent.side)
            .price(intent.price)
            .size(intent.size.trunc_with_scale(2))
            .order_type(intent.order_type.clone())
            .post_only(intent.post_only)
            .build()
            .await
        {
            Ok(s) => s,
            Err(e) => {
                results[i] = Some(Err(e.into()));
                continue;
            }
        };

        match client.sign(&*ctx.signer, signable).await {
            Ok(signed) => {
                signed_orders.push(signed);
                valid_indices.push(i);
            }
            Err(e) => {
                results[i] = Some(Err(e.into()));
            }
        }
    }

    if signed_orders.is_empty() {
        return results.into_iter().map(|r| r.unwrap()).collect();
    }

    // Submit batch
    match client.post_orders(signed_orders).await {
        Ok(responses) => {
            for (resp, &idx) in responses.iter().zip(valid_indices.iter()) {
                let matched = resp.status == polymarket_client_sdk::clob::types::OrderStatusType::Matched;
                debug!(
                    order_id = %resp.order_id,
                    side = ?intents[idx].side,
                    price = %intents[idx].price,
                    "order: batch placed"
                );
                results[idx] = Some(Ok(OrderResult {
                    order_id: resp.order_id.clone(),
                    intent: intents[idx].clone(),
                    matched,
                    making_amount: resp.making_amount,
                    taking_amount: resp.taking_amount,
                }));
            }
        }
        Err(e) => {
            // Batch failed — mark all valid orders as failed
            for &idx in &valid_indices {
                if results[idx].is_none() {
                    results[idx] = Some(Err(anyhow::anyhow!("batch post failed: {e}")));
                }
            }
        }
    }

    results.into_iter().map(|r| r.unwrap_or_else(|| Err(anyhow::anyhow!("unreachable")))).collect()
}

/// Cancel a specific order.
#[allow(dead_code)]
pub async fn cancel(client: &AuthClient, order_id: &str) -> Result<()> {
    client.cancel_order(order_id).await?;
    debug!(order_id, "order: cancelled");
    Ok(())
}

/// Emergency cancel-all.
pub async fn cancel_all(client: &AuthClient) -> Result<()> {
    let _resp = client.cancel_all_orders().await?;
    warn!("order: cancel-all executed");
    Ok(())
}
