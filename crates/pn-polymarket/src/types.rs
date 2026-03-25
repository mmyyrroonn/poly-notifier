//! Shared types for Polymarket API responses and WebSocket events.

use std::str::FromStr;

use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize};

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

/// Liquidity reward settings attached to a Gamma market summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GammaClobReward {
    /// Reward configuration identifier.
    pub id: String,
    /// Polymarket condition identifier.
    pub condition_id: String,
    /// Reward paid per day for this configuration.
    pub daily_rate: Decimal,
    /// Reward start date, if present.
    pub start_date: Option<String>,
    /// Reward end date, if present.
    pub end_date: Option<String>,
}

/// A reward-enriched market summary returned by the Gamma `/markets` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GammaMarketSummary {
    /// Polymarket condition identifier (hex string).
    pub condition_id: String,
    /// The market question text.
    pub question: String,
    /// URL slug for this market.
    pub slug: String,
    /// Parsed token/outcome pairs, one per outcome.
    pub tokens: Vec<GammaToken>,
    /// Whether the market is currently open for trading.
    pub active: bool,
    /// Whether the market is already closed.
    pub closed: bool,
    /// Current best bid, if populated by the API.
    pub best_bid: Option<Decimal>,
    /// Current best ask, if populated by the API.
    pub best_ask: Option<Decimal>,
    /// Current spread, if populated by the API.
    pub spread: Option<Decimal>,
    /// Current CLOB liquidity, if populated by the API.
    pub liquidity_clob: Option<Decimal>,
    /// Trailing 24h CLOB volume, if populated by the API.
    pub volume_24hr_clob: Option<Decimal>,
    /// Polymarket's competitiveness score, if populated by the API.
    pub competitive: Option<Decimal>,
    /// Minimum qualifying size for rewards, if present.
    pub rewards_min_size: Option<Decimal>,
    /// Maximum qualifying spread in cents, if present.
    pub rewards_max_spread: Option<Decimal>,
    /// Attached CLOB reward windows.
    pub clob_rewards: Vec<GammaClobReward>,
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

/// Raw market summary object returned by the Gamma `/markets` endpoint.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GammaRawMarketSummary {
    pub condition_id: Option<String>,
    pub question: Option<String>,
    pub slug: Option<String>,
    pub outcomes: Option<String>,
    pub clob_token_ids: Option<String>,
    pub active: Option<bool>,
    pub closed: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_option_decimal")]
    pub best_bid: Option<Decimal>,
    #[serde(default, deserialize_with = "deserialize_option_decimal")]
    pub best_ask: Option<Decimal>,
    #[serde(default, deserialize_with = "deserialize_option_decimal")]
    pub spread: Option<Decimal>,
    #[serde(default, deserialize_with = "deserialize_option_decimal")]
    pub liquidity_clob: Option<Decimal>,
    #[serde(
        default,
        rename = "volume24hrClob",
        deserialize_with = "deserialize_option_decimal"
    )]
    pub volume_24hr_clob: Option<Decimal>,
    #[serde(default, deserialize_with = "deserialize_option_decimal")]
    pub competitive: Option<Decimal>,
    #[serde(default, deserialize_with = "deserialize_option_decimal")]
    pub rewards_min_size: Option<Decimal>,
    #[serde(default, deserialize_with = "deserialize_option_decimal")]
    pub rewards_max_spread: Option<Decimal>,
    pub clob_rewards: Option<Vec<GammaRawClobReward>>,
}

/// Raw reward summary attached to a Gamma market summary.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GammaRawClobReward {
    pub id: Option<String>,
    pub condition_id: Option<String>,
    #[serde(deserialize_with = "deserialize_decimal")]
    pub rewards_daily_rate: Decimal,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
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

/// A single price level in the public REST order book.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublicBookLevel {
    /// Price level as a decimal fraction.
    pub price: Decimal,
    /// Size available at this price level.
    pub size: Decimal,
}

/// Parsed public REST order book for one outcome token.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublicOrderBook {
    /// Condition identifier for the book's market.
    pub market: String,
    /// Outcome token identifier.
    pub asset_id: String,
    /// Exchange-provided timestamp string.
    pub timestamp: String,
    /// Book hash.
    pub hash: String,
    /// Bid levels, normalized descending by price.
    pub bids: Vec<PublicBookLevel>,
    /// Ask levels, normalized ascending by price.
    pub asks: Vec<PublicBookLevel>,
    /// Exchange minimum order size.
    pub min_order_size: Decimal,
    /// Book tick size.
    pub tick_size: Decimal,
    /// Whether the book is on a negative-risk market.
    pub neg_risk: bool,
    /// Last trade price, if present.
    pub last_trade_price: Option<Decimal>,
}

/// Raw public order-book response from `GET /book`.
#[derive(Debug, Deserialize)]
pub(crate) struct RawPublicOrderBook {
    pub market: String,
    pub asset_id: String,
    pub timestamp: String,
    pub hash: String,
    pub bids: Vec<RawBookLevel>,
    pub asks: Vec<RawBookLevel>,
    #[serde(deserialize_with = "deserialize_decimal")]
    pub min_order_size: Decimal,
    #[serde(deserialize_with = "deserialize_decimal")]
    pub tick_size: Decimal,
    pub neg_risk: bool,
    #[serde(default, deserialize_with = "deserialize_option_decimal")]
    pub last_trade_price: Option<Decimal>,
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

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DecimalRepr {
    Integer(i64),
    Unsigned(u64),
    Float(f64),
    String(String),
}

pub(crate) fn deserialize_decimal<'de, D>(deserializer: D) -> std::result::Result<Decimal, D::Error>
where
    D: Deserializer<'de>,
{
    let value = DecimalRepr::deserialize(deserializer)?;
    decimal_from_repr(value).map_err(serde::de::Error::custom)
}

pub(crate) fn deserialize_option_decimal<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Decimal>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<DecimalRepr>::deserialize(deserializer)?;
    value
        .map(decimal_from_repr)
        .transpose()
        .map_err(serde::de::Error::custom)
}

fn decimal_from_repr(value: DecimalRepr) -> anyhow::Result<Decimal> {
    match value {
        DecimalRepr::Integer(value) => Ok(Decimal::from(value)),
        DecimalRepr::Unsigned(value) => Ok(Decimal::from(value)),
        DecimalRepr::Float(value) => Decimal::from_str(&value.to_string())
            .map_err(|error| anyhow::anyhow!("invalid decimal float '{value}': {error}")),
        DecimalRepr::String(value) => Decimal::from_str(&value)
            .map_err(|error| anyhow::anyhow!("invalid decimal string '{value}': {error}")),
    }
}
