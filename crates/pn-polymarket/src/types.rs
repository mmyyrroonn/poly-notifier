//! Shared types for Polymarket API responses and WebSocket events.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Gamma API types
// ---------------------------------------------------------------------------

/// A single outcome token within a Polymarket market.
///
/// # Example
///
/// ```rust
/// use pn_polymarket::types::GammaToken;
///
/// let token = GammaToken {
///     token_id: "0xabc123".to_string(),
///     outcome: "Yes".to_string(),
/// };
/// assert_eq!(token.outcome, "Yes");
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GammaToken {
    /// CLOB token identifier (outcome token address).
    pub token_id: String,
    /// Human-readable outcome label, e.g. `"Yes"` or `"No"`.
    pub outcome: String,
}

/// A Polymarket prediction market as returned by the Gamma API.
///
/// Outcomes and token IDs are stored in the raw JSON-array-string format
/// produced by the Gamma API (e.g. `"[\"Yes\",\"No\"]"`).  The
/// [`tokens`](GammaMarket::tokens) field provides a parsed, zipped view.
///
/// # Example
///
/// ```rust
/// use pn_polymarket::types::{GammaMarket, GammaToken};
///
/// let market = GammaMarket {
///     condition_id: "0xcond".to_string(),
///     question: "Will it rain tomorrow?".to_string(),
///     slug: "will-it-rain-tomorrow".to_string(),
///     outcomes: r#"["Yes","No"]"#.to_string(),
///     tokens: vec![
///         GammaToken { token_id: "0xa".to_string(), outcome: "Yes".to_string() },
///         GammaToken { token_id: "0xb".to_string(), outcome: "No".to_string() },
///     ],
///     active: true,
/// };
/// assert_eq!(market.tokens.len(), 2);
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GammaMarket {
    /// Polymarket condition identifier (hex string).
    pub condition_id: String,
    /// The market question text.
    pub question: String,
    /// URL slug for this market.
    pub slug: String,
    /// Raw JSON-array string of outcome labels, e.g. `"[\"Yes\",\"No\"]"`.
    pub outcomes: String,
    /// Parsed token/outcome pairs, one per outcome.
    pub tokens: Vec<GammaToken>,
    /// Whether the market is currently open for trading.
    pub active: bool,
}

// ---------------------------------------------------------------------------
// Raw Gamma API wire types (internal)
// ---------------------------------------------------------------------------

/// Raw event object as returned by the Gamma `/events` endpoint.
#[derive(Debug, Deserialize)]
pub(crate) struct GammaEvent {
    #[allow(dead_code)]
    pub title: Option<String>,
    pub slug: Option<String>,
    pub markets: Option<Vec<GammaRawMarket>>,
}

/// Raw market object nested inside a [`GammaEvent`].
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GammaRawMarket {
    pub condition_id: Option<String>,
    pub question: Option<String>,
    pub outcomes: Option<String>,
    pub clob_token_ids: Option<String>,
    pub active: Option<bool>,
    pub slug: Option<String>,
}

// ---------------------------------------------------------------------------
// CLOB REST API wire types
// ---------------------------------------------------------------------------

/// Response body for `GET /midpoint?token_id={id}`.
#[derive(Debug, Deserialize)]
pub(crate) struct MidpointResponse {
    pub mid: String,
}

/// Response body for `GET /price?token_id={id}&side=buy`.
#[derive(Debug, Deserialize)]
pub(crate) struct PriceResponse {
    pub price: String,
}

// ---------------------------------------------------------------------------
// WebSocket event types
// ---------------------------------------------------------------------------

/// A single level in an order-book snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BookLevel {
    /// Price level as a decimal fraction.
    pub price: Decimal,
    /// Size available at this price level.
    pub size: Decimal,
}

/// Parsed, domain-typed representation of a single WebSocket event.
///
/// The Polymarket WebSocket delivers JSON arrays where each element may
/// represent a different event type.
///
/// # Example
///
/// ```rust
/// use pn_polymarket::types::WsEvent;
/// use rust_decimal_macros::dec;
///
/// let event = WsEvent::PriceChange {
///     asset_id: "0xtoken".to_string(),
///     price: dec!(0.73),
/// };
/// match event {
///     WsEvent::PriceChange { asset_id, price } => {
///         println!("{asset_id} @ {price}");
///     }
///     _ => {}
/// }
/// ```
#[derive(Debug, Clone)]
pub enum WsEvent {
    /// A price has changed for an outcome token.
    PriceChange {
        /// The outcome token whose price changed.
        asset_id: String,
        /// New mid-price as a decimal fraction (0..=1).
        price: Decimal,
    },
    /// The most recent trade executed at this price.
    LastTradePrice {
        /// The outcome token traded.
        asset_id: String,
        /// Price at which the last trade occurred.
        price: Decimal,
    },
    /// Full order-book snapshot for an outcome token.
    Book {
        /// The outcome token this book belongs to.
        asset_id: String,
        /// Bid levels (descending by price).
        bids: Vec<BookLevel>,
        /// Ask levels (ascending by price).
        asks: Vec<BookLevel>,
    },
    /// An unrecognised event; the raw JSON text is preserved.
    Unknown(String),
}

// ---------------------------------------------------------------------------
// Raw WebSocket wire types (internal)
// ---------------------------------------------------------------------------

/// Raw event element received from the WebSocket.
#[derive(Debug, Deserialize)]
pub(crate) struct RawWsEvent {
    pub event_type: Option<String>,
    pub asset_id: Option<String>,
    pub price: Option<String>,
    pub last_trade_price: Option<String>,
    pub bids: Option<Vec<RawBookLevel>>,
    pub asks: Option<Vec<RawBookLevel>>,
}

/// Raw bid/ask level in a book snapshot.
#[derive(Debug, Deserialize)]
pub(crate) struct RawBookLevel {
    pub price: String,
    pub size: String,
}
