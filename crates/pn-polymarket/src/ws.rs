//! Polymarket WebSocket client for real-time price updates.
//!
//! [`PolymarketWs`] opens a TLS WebSocket connection to the Polymarket CLOB
//! market feed, sends a subscribe message, and forwards parsed [`WsEvent`]s
//! to the caller via an [`mpsc::UnboundedSender`].  Reconnection and backoff
//! logic are intentionally left to the caller (see `pn-monitor`'s
//! [`ConnectionManager`]).
//!
//! # Protocol
//!
//! 1. Connect to `wss://ws-subscriptions-clob.polymarket.com/ws/market`.
//! 2. Send `{"type":"market","assets_ids":["id1","id2"]}`.
//! 3. Read incoming JSON-array text frames; each element is an event object.
//! 4. Send periodic `Ping` frames to keep the TCP connection alive.
//!
//! # Example
//!
//! ```no_run
//! use std::time::Duration;
//! use tokio::sync::mpsc;
//! use tokio_util::sync::CancellationToken;
//! use pn_polymarket::ws::PolymarketWs;
//!
//! # #[tokio::main]
//! # async fn main() -> anyhow::Result<()> {
//! let ws = PolymarketWs::with_ping_interval(Duration::from_secs(10));
//! let token_ids = vec!["0xtoken_a".to_string()];
//! let (tx, mut rx) = mpsc::unbounded_channel();
//! let cancel = CancellationToken::new();
//!
//! // In production this would run in a spawned task.
//! // ws.run(&token_ids, tx, cancel).await?;
//! # Ok(())
//! # }
//! ```

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{interval, MissedTickBehavior};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::types::{BookLevel, RawBookLevel, RawWsEvent, WsEvent};

const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

/// WebSocket client for the Polymarket market price stream.
///
/// Obtain an instance with [`new`](PolymarketWs::new) or
/// [`with_ping_interval`](PolymarketWs::with_ping_interval), then call
/// [`run`](PolymarketWs::run) to drive the connection.
#[derive(Debug, Clone)]
pub struct PolymarketWs {
    /// How often a WebSocket `Ping` frame is sent to keep the connection live.
    pub ping_interval: Duration,
}

impl PolymarketWs {
    /// Create a [`PolymarketWs`] with the default 10-second ping interval.
    pub fn new() -> Self {
        Self::with_ping_interval(Duration::from_secs(10))
    }

    /// Create a [`PolymarketWs`] with a custom ping interval.
    pub fn with_ping_interval(ping_interval: Duration) -> Self {
        Self { ping_interval }
    }

    /// Connect to the Polymarket WebSocket, subscribe to `token_ids`, and
    /// forward parsed [`WsEvent`]s to `event_tx` until `cancel` fires or
    /// the remote end closes the connection.
    ///
    /// This method returns (rather than panicking) on any network or protocol
    /// error, allowing the caller to implement reconnect logic.
    ///
    /// # Errors
    ///
    /// Returns an error if the initial TCP connection or subscribe handshake
    /// fails.  Read errors encountered after that cause the method to return
    /// `Ok(())` so the caller can reconnect.
    pub async fn run(
        &self,
        token_ids: &[String],
        event_tx: mpsc::UnboundedSender<WsEvent>,
        cancel: CancellationToken,
    ) -> Result<()> {
        info!(url = WS_URL, tokens = token_ids.len(), "PolymarketWs: connecting");

        let (ws_stream, _) = connect_async(WS_URL)
            .await
            .context("WebSocket connect failed")?;

        info!("PolymarketWs: connected");

        let (mut write, mut read) = ws_stream.split();

        // Send the subscribe message immediately after connect.
        let subscribe = build_subscribe_message(token_ids);
        write
            .send(Message::Text(subscribe.into()))
            .await
            .context("failed to send subscribe message")?;
        debug!("PolymarketWs: subscribe sent");

        // Periodic ping ticker.
        let mut ping_ticker = interval(self.ping_interval);
        ping_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Skip the first immediate tick.
        ping_ticker.tick().await;

        loop {
            tokio::select! {
                biased;

                // Graceful shutdown.
                () = cancel.cancelled() => {
                    info!("PolymarketWs: cancelled, closing");
                    let _ = write.close().await;
                    break;
                }

                // Keep-alive ping.
                _ = ping_ticker.tick() => {
                    if let Err(e) = write.send(Message::Ping(vec![].into())).await {
                        warn!(error=%e, "PolymarketWs: ping send failed, treating as disconnect");
                        break;
                    }
                    debug!("PolymarketWs: ping sent");
                }

                // Incoming message.
                maybe_msg = read.next() => {
                    match maybe_msg {
                        None => {
                            info!("PolymarketWs: stream closed by remote");
                            break;
                        }
                        Some(Err(e)) => {
                            warn!(error=%e, "PolymarketWs: read error, treating as disconnect");
                            break;
                        }
                        Some(Ok(msg)) => {
                            match msg {
                                Message::Text(text) => {
                                    for event in parse_ws_message(&text) {
                                        // If the receiver is gone we can stop.
                                        if event_tx.send(event).is_err() {
                                            info!("PolymarketWs: event receiver dropped");
                                            return Ok(());
                                        }
                                    }
                                }
                                Message::Ping(data) => {
                                    // Respond to server-initiated pings.
                                    let _ = write.send(Message::Pong(data)).await;
                                }
                                Message::Pong(_) => {
                                    debug!("PolymarketWs: pong received");
                                }
                                Message::Close(frame) => {
                                    info!(?frame, "PolymarketWs: close frame received");
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

impl Default for PolymarketWs {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Build the Polymarket subscribe JSON message.
fn build_subscribe_message(token_ids: &[String]) -> String {
    serde_json::json!({
        "type": "market",
        "assets_ids": token_ids,
    })
    .to_string()
}

/// Parse a raw WebSocket text frame into zero or more [`WsEvent`]s.
fn parse_ws_message(text: &str) -> Vec<WsEvent> {
    let raw_value: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            warn!(error=%e, "PolymarketWs: failed to parse frame as JSON");
            return vec![WsEvent::Unknown(text.to_string())];
        }
    };

    let arr = match raw_value.as_array() {
        Some(a) => a,
        None => return vec![WsEvent::Unknown(text.to_string())],
    };

    arr.iter().map(parse_single_event).collect()
}

/// Convert one raw JSON value into a [`WsEvent`].
fn parse_single_event(value: &Value) -> WsEvent {
    let raw: RawWsEvent = match serde_json::from_value(value.clone()) {
        Ok(r) => r,
        Err(e) => {
            warn!(error=%e, "PolymarketWs: failed to deserialise event element");
            return WsEvent::Unknown(value.to_string());
        }
    };

    let asset_id = match raw.asset_id.filter(|s| !s.is_empty()) {
        Some(id) => id,
        None => return WsEvent::Unknown(value.to_string()),
    };

    match raw.event_type.as_deref().unwrap_or("") {
        "price_change" => {
            match raw.price.as_deref().and_then(|p| Decimal::from_str(p).ok()) {
                Some(price) => WsEvent::PriceChange { asset_id, price },
                None => {
                    warn!(%asset_id, "PolymarketWs: price_change has unparseable price");
                    WsEvent::Unknown(value.to_string())
                }
            }
        }
        "last_trade_price" => {
            let price_str = raw.last_trade_price.as_deref().or(raw.price.as_deref());
            match price_str.and_then(|p| Decimal::from_str(p).ok()) {
                Some(price) => WsEvent::LastTradePrice { asset_id, price },
                None => {
                    warn!(%asset_id, "PolymarketWs: last_trade_price has unparseable price");
                    WsEvent::Unknown(value.to_string())
                }
            }
        }
        "book" => {
            let bids = parse_book_levels(raw.bids.as_deref().unwrap_or(&[]));
            let asks = parse_book_levels(raw.asks.as_deref().unwrap_or(&[]));
            WsEvent::Book { asset_id, bids, asks }
        }
        _ => WsEvent::Unknown(value.to_string()),
    }
}

/// Convert raw book levels, skipping entries with unparseable price or size.
fn parse_book_levels(raw: &[RawBookLevel]) -> Vec<BookLevel> {
    raw.iter()
        .filter_map(|r| {
            let price = Decimal::from_str(&r.price).ok()?;
            let size = Decimal::from_str(&r.size).ok()?;
            Some(BookLevel { price, size })
        })
        .collect()
}
