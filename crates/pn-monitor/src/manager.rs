use std::collections::HashMap;
use std::time::Duration;

use chrono::Utc;
use rust_decimal::Decimal;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use pn_common::events::PriceUpdate;
use pn_polymarket::types::WsEvent;
use pn_polymarket::ws::PolymarketWs;

pub struct ConnectionManager {
    ws_ping_interval: Duration,
    reconnect_base_delay: Duration,
    reconnect_max_delay: Duration,
}

impl ConnectionManager {
    pub fn new(
        ws_ping_interval: Duration,
        reconnect_base_delay: Duration,
        reconnect_max_delay: Duration,
    ) -> Self {
        Self {
            ws_ping_interval,
            reconnect_base_delay,
            reconnect_max_delay,
        }
    }

    /// Run the WS connection loop with automatic reconnection.
    /// Converts WsEvents to PriceUpdates enriched with condition_id,
    /// then broadcasts them.
    pub async fn run(
        &self,
        token_ids: Vec<String>,
        token_to_condition: HashMap<String, String>,
        price_tx: broadcast::Sender<PriceUpdate>,
        cancel: CancellationToken,
    ) {
        if token_ids.is_empty() {
            info!("ConnectionManager: no token_ids, idling until cancellation");
            cancel.cancelled().await;
            return;
        }

        let mut attempt = 0u32;

        loop {
            if cancel.is_cancelled() {
                return;
            }

            info!(
                tokens = token_ids.len(),
                attempt, "ConnectionManager: starting WS session"
            );

            let ws = PolymarketWs::with_ping_interval(self.ws_ping_interval);
            let (event_tx, mut event_rx) = mpsc::unbounded_channel::<WsEvent>();
            let ws_cancel = cancel.clone();
            let token_ids_clone = token_ids.clone();

            // Spawn the WS task
            let ws_handle = tokio::spawn(async move {
                ws.run(&token_ids_clone, event_tx, ws_cancel).await
            });

            let mut received_any = false;

            // Read WsEvents from the channel and convert to PriceUpdates
            loop {
                tokio::select! {
                    biased;

                    () = cancel.cancelled() => {
                        ws_handle.abort();
                        return;
                    }

                    event = event_rx.recv() => {
                        match event {
                            Some(ws_event) => {
                                if let Some(update) = ws_event_to_price_update(
                                    &ws_event,
                                    &token_to_condition,
                                ) {
                                    received_any = true;
                                    let _ = price_tx.send(update);
                                }
                            }
                            None => {
                                // WS task closed the sender
                                break;
                            }
                        }
                    }
                }
            }

            // Wait for WS task to complete
            match ws_handle.await {
                Ok(Ok(())) => info!("ConnectionManager: WS session ended cleanly"),
                Ok(Err(e)) => warn!(error=%e, "ConnectionManager: WS session error"),
                Err(e) if e.is_cancelled() => return,
                Err(e) => warn!(error=%e, "ConnectionManager: WS task panicked"),
            }

            if cancel.is_cancelled() {
                return;
            }

            // Reset backoff on successful connection
            if received_any {
                attempt = 0;
            }

            let delay = self.calculate_backoff(attempt);
            attempt = attempt.saturating_add(1);
            warn!(
                delay_secs = delay.as_secs(),
                "ConnectionManager: reconnecting after backoff"
            );

            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                () = cancel.cancelled() => return,
            }
        }
    }

    fn calculate_backoff(&self, attempt: u32) -> Duration {
        let delay = self.reconnect_base_delay * 2u32.saturating_pow(attempt);
        delay.min(self.reconnect_max_delay)
    }
}

fn ws_event_to_price_update(
    event: &WsEvent,
    token_to_condition: &HashMap<String, String>,
) -> Option<PriceUpdate> {
    let (asset_id, price): (&str, Decimal) = match event {
        WsEvent::PriceChange { asset_id, price } => (asset_id.as_str(), *price),
        WsEvent::LastTradePrice { asset_id, price } => (asset_id.as_str(), *price),
        _ => return None,
    };

    let condition_id = token_to_condition
        .get(asset_id)
        .cloned()
        .unwrap_or_default();

    Some(PriceUpdate {
        token_id: asset_id.to_string(),
        condition_id,
        price,
        timestamp: Utc::now(),
    })
}
