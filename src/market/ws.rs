use std::sync::Arc;

use futures::StreamExt;
use polymarket_client_sdk::clob::ws::{Client as WsClient, types::response::BookUpdate};
use polymarket_client_sdk::types::U256;
use rand::Rng;
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use crate::config::Config;
use crate::market::discovery::TradableMarket;

/// Events emitted by the WS feed to downstream consumers.
#[derive(Debug, Clone)]
pub enum FeedEvent {
    BookSnapshot {
        asset_id: String,
        update: BookUpdate,
    },
}

/// Spawn the WebSocket feed manager.
///
/// Watches the discovery universe and subscribes to orderbook streams
/// for all active tokens. Emits FeedEvents on the returned channel.
pub fn spawn(
    config: &Config,
    universe_rx: watch::Receiver<Arc<Vec<TradableMarket>>>,
) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedReceiver<FeedEvent>) {
    let _stale_threshold = config.stale_feed_threshold;
    let (event_tx, event_rx) = mpsc::unbounded_channel();

    let handle = tokio::spawn(async move {
        let ws_client = WsClient::default();
        let mut current_ids: Vec<String> = Vec::new();

        // Watch for universe changes and re-subscribe
        let mut uni_rx = universe_rx;
        loop {
            // Collect all token IDs from the current universe
            let universe = uni_rx.borrow_and_update().clone();
            let new_ids: Vec<String> = universe
                .iter()
                .flat_map(|m| m.tokens.iter().map(|t| t.token_id.clone()))
                .collect();

            if new_ids != current_ids && !new_ids.is_empty() {
                current_ids = new_ids.clone();

                let asset_ids: Vec<U256> = current_ids
                    .iter()
                    .filter_map(|id| id.parse::<U256>().ok())
                    .collect();

                info!(count = asset_ids.len(), "ws: subscribing to orderbook streams");

                match ws_client.subscribe_orderbook(asset_ids) {
                    Ok(stream) => {
                        let tx = event_tx.clone();
                        let mut stream = Box::pin(stream);

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
                                        }
                                        None => {
                                            warn!("ws: stream ended");
                                            // Reconnect stagger: sleep 1-3s jitter before resubscribing
                                            // to avoid thundering herd on reconnect.
                                            let jitter_ms = rand::rng().random_range(1000..=3000);
                                            tokio::time::sleep(std::time::Duration::from_millis(jitter_ms)).await;
                                            // TODO: compare book hashes against REST snapshot
                                            // before resuming quoting (hash-check-vs-REST resync).
                                            break;
                                        }
                                    }
                                }
                                _ = uni_rx.changed() => {
                                    info!("ws: universe changed — resubscribing");
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("ws: subscribe failed: {e}");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
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
