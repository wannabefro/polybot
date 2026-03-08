use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

use futures::StreamExt;
use polymarket_client_sdk::clob::ws::{types::response::BookUpdate, Client as WsClient};
use polymarket_client_sdk::clob::{
    types::{request::OrderBookSummaryRequest, response::OrderBookSummaryResponse},
    Client as RestClient, Config as RestConfig,
};
use polymarket_client_sdk::types::U256;
use rand::Rng;
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::market::discovery::TradableMarket;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResyncFailureReason {
    InvalidAssetIds,
    SubscribeFailed,
    SnapshotFetchFailed,
    AuthoritativeRestoreFailed,
}

/// Events emitted by the WS feed to downstream consumers.
#[derive(Debug, Clone)]
pub enum FeedEvent {
    ResyncStarted,
    ResyncFailed {
        reason: ResyncFailureReason,
    },
    AuthoritativeSnapshots {
        asset_ids: Vec<String>,
        books: Vec<OrderBookSummaryResponse>,
    },
    BookSnapshot {
        asset_id: String,
        update: BookUpdate,
    },
}

async fn fetch_authoritative_books(
    client: &RestClient,
    asset_ids: &[String],
) -> Option<Vec<OrderBookSummaryResponse>> {
    let requests: Vec<OrderBookSummaryRequest> = match asset_ids
        .iter()
        .map(|asset_id| {
            U256::from_str(asset_id).ok().map(|token_id| {
                OrderBookSummaryRequest::builder()
                    .token_id(token_id)
                    .build()
            })
        })
        .collect::<Option<Vec<_>>>()
    {
        Some(requests) => requests,
        None => {
            warn!("ws: failed to parse one or more asset ids for authoritative resync");
            return None;
        }
    };

    let expected: HashSet<String> = asset_ids.iter().cloned().collect();
    for (attempt, backoff_secs) in [1_u64, 2, 4].into_iter().enumerate() {
        match client.order_books(&requests).await {
            Ok(books) => {
                let returned: HashSet<String> =
                    books.iter().map(|book| book.asset_id.to_string()).collect();
                if returned == expected && books.len() == requests.len() {
                    return Some(books);
                }

                warn!(
                    attempt = attempt + 1,
                    expected = requests.len(),
                    received = books.len(),
                    "ws: authoritative resync returned incomplete book set"
                );
            }
            Err(e) => {
                warn!(
                    attempt = attempt + 1,
                    err = %e,
                    "ws: authoritative resync snapshot fetch failed"
                );
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
    }

    None
}

fn jitter_retry_delay() -> std::time::Duration {
    let jitter_ms = rand::rng().random_range(1000..=3000);
    std::time::Duration::from_millis(jitter_ms)
}

/// Spawn the WebSocket feed manager.
///
/// Watches the discovery universe and subscribes to orderbook streams
/// for all active tokens. Emits FeedEvents on the returned channel.
pub fn spawn(
    config: &Config,
    universe_rx: watch::Receiver<Arc<Vec<TradableMarket>>>,
) -> (
    tokio::task::JoinHandle<()>,
    mpsc::UnboundedReceiver<FeedEvent>,
) {
    let _stale_threshold = config.stale_feed_threshold;
    let max_ws_tokens = config.max_ws_tokens;
    let clob_host = config.clob_host.clone();
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    let handle = tokio::spawn(async move {
        let ws_client = WsClient::default();
        let rest_client = match RestClient::new(&clob_host, RestConfig::default()) {
            Ok(client) => client,
            Err(e) => {
                error!(err = %e, "ws: failed to build REST client for authoritative resync");
                return;
            }
        };
        let mut current_ids: Vec<String> = Vec::new();
        let mut need_resubscribe = true;

        // Watch for universe changes and re-subscribe
        let mut uni_rx = universe_rx;
        loop {
            // Collect all token IDs from the current universe.
            // Cap to max_ws_tokens to avoid overwhelming the WS server.
            // Prioritize rewards-active markets (most profitable for small accounts).
            let universe = uni_rx.borrow_and_update().clone();
            let mut sorted: Vec<&TradableMarket> = universe.iter().collect();
            sorted.sort_by(|a, b| {
                b.rewards_active.cmp(&a.rewards_active).then(
                    b.volume_24h
                        .partial_cmp(&a.volume_24h)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
            });
            let new_ids: Vec<String> = sorted
                .iter()
                .flat_map(|m| m.tokens.iter().map(|t| t.token_id.clone()))
                .take(max_ws_tokens)
                .collect();

            if !new_ids.is_empty() && (need_resubscribe || new_ids != current_ids) {
                current_ids = new_ids.clone();
                let _ = event_tx.send(FeedEvent::ResyncStarted);

                let asset_ids: Vec<U256> = current_ids
                    .iter()
                    .filter_map(|id| id.parse::<U256>().ok())
                    .collect();

                if asset_ids.len() != current_ids.len() {
                    warn!(
                        expected = current_ids.len(),
                        parsed = asset_ids.len(),
                        "ws: invalid asset ids prevent authoritative resync"
                    );
                    let _ = event_tx.send(FeedEvent::ResyncFailed {
                        reason: ResyncFailureReason::InvalidAssetIds,
                    });
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    need_resubscribe = true;
                    continue;
                }

                debug!(
                    count = asset_ids.len(),
                    "ws: subscribing to orderbook streams"
                );

                match ws_client.subscribe_orderbook(asset_ids) {
                    Ok(stream) => {
                        let tx = event_tx.clone();
                        let mut stream = Box::pin(stream);

                        if let Some(books) =
                            fetch_authoritative_books(&rest_client, &current_ids).await
                        {
                            let _ = tx.send(FeedEvent::AuthoritativeSnapshots {
                                asset_ids: current_ids.clone(),
                                books,
                            });
                        } else {
                            warn!("ws: authoritative resync failed; keeping quoting paused");
                            let _ = tx.send(FeedEvent::ResyncFailed {
                                reason: ResyncFailureReason::SnapshotFetchFailed,
                            });
                            tokio::time::sleep(jitter_retry_delay()).await;
                            need_resubscribe = true;
                            continue;
                        }

                        // Process stream until universe changes
                        loop {
                            tokio::select! {
                                msg = stream.next() => {
                                    match msg {
                                        Some(Ok(book)) => {
                                            let asset_id = book.asset_id.to_string();
                                            let _ = tx.send(FeedEvent::BookSnapshot {
                                                asset_id,
                                                update: book,
                                            });
                                        }
                                        Some(Err(e)) => {
                                            warn!("ws: stream error: {e}");
                                            let _ = tx.send(FeedEvent::ResyncStarted);
                                            need_resubscribe = true;
                                            tokio::time::sleep(jitter_retry_delay()).await;
                                            break;
                                        }
                                        None => {
                                            warn!("ws: stream ended");
                                            let _ = tx.send(FeedEvent::ResyncStarted);
                                            need_resubscribe = true;
                                            // Reconnect stagger: sleep 1-3s jitter before resubscribing
                                            // to avoid thundering herd on reconnect.
                                            tokio::time::sleep(jitter_retry_delay()).await;
                                            break;
                                        }
                                    }
                                }
                                _ = uni_rx.changed() => {
                                    let _ = tx.send(FeedEvent::ResyncStarted);
                                    need_resubscribe = true;
                                    info!("ws: universe changed — resubscribing");
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("ws: subscribe failed: {e}");
                        let _ = event_tx.send(FeedEvent::ResyncFailed {
                            reason: ResyncFailureReason::SubscribeFailed,
                        });
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        need_resubscribe = true;
                    }
                }
            } else {
                // No tokens or no change — wait for universe update
                if uni_rx.changed().await.is_err() {
                    info!("ws: universe sender dropped — exiting");
                    return;
                }
            }
        }
    });

    (handle, event_rx)
}
