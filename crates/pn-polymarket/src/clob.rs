//! CLOB REST API client for Polymarket price queries.
//!
//! The CLOB (Central Limit Order Book) API exposes per-token price endpoints.
//! This module wraps them with parallel fan-out for batched queries.
//!
//! # Example
//!
//! ```no_run
//! use pn_polymarket::clob::ClobClient;
//!
//! # #[tokio::main]
//! # async fn main() -> anyhow::Result<()> {
//! let client = ClobClient::new();
//! let token_ids = vec![
//!     "0xtoken_a".to_string(),
//!     "0xtoken_b".to_string(),
//! ];
//!
//! // Fetch mid-prices for both tokens in parallel.
//! let midpoints = client.get_midpoints(&token_ids).await?;
//! for (id, price) in &midpoints {
//!     println!("{id} mid = {price}");
//! }
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::str::FromStr;

use anyhow::{Context, Result};
use futures_util::future::join_all;
use reqwest::Client;
use rust_decimal::Decimal;
use tracing::{debug, instrument, warn};

use crate::types::{
    MidpointResponse, PriceResponse, PublicBookLevel, PublicOrderBook, RawPublicOrderBook,
};

const DEFAULT_BASE_URL: &str = "https://clob.polymarket.com";

/// HTTP client for the Polymarket CLOB REST API.
#[derive(Clone)]
pub struct ClobClient {
    http: Client,
    base_url: String,
}

impl ClobClient {
    /// Create a new [`ClobClient`] pointing at the production CLOB API.
    pub fn new() -> Self {
        Self {
            http: Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Create a client with a custom base URL (useful for testing).
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.into(),
        }
    }

    /// Fetch mid-prices for multiple tokens in parallel.
    ///
    /// Issues one `GET /midpoint?token_id={id}` request per token concurrently
    /// and returns a map from token ID to mid-price.  Tokens whose requests
    /// fail are logged and omitted from the result rather than propagating an
    /// error, so a single bad token ID does not abort the entire batch.
    ///
    /// # Errors
    ///
    /// Returns an error only if the Tokio task machinery fails.
    #[instrument(skip(self, token_ids), fields(count = token_ids.len()))]
    pub async fn get_midpoints(&self, token_ids: &[String]) -> Result<HashMap<String, Decimal>> {
        let futures: Vec<_> = token_ids
            .iter()
            .map(|id| self.fetch_midpoint(id.clone()))
            .collect();

        let results: Vec<Result<Decimal>> = join_all(futures).await;

        let mut map = HashMap::with_capacity(token_ids.len());
        for (token_id, result) in token_ids.iter().zip(results.into_iter()) {
            match result {
                Ok(price) => {
                    map.insert(token_id.clone(), price);
                }
                Err(e) => {
                    warn!(token_id, error=%e, "failed to fetch midpoint, skipping");
                }
            }
        }
        Ok(map)
    }

    #[instrument(skip(self, token_ids), fields(count = token_ids.len()))]
    pub async fn get_prices(&self, token_ids: &[String]) -> Result<HashMap<String, Decimal>> {
        let futures: Vec<_> = token_ids
            .iter()
            .map(|id| self.fetch_price(id.clone()))
            .collect();

        let results: Vec<Result<Decimal>> = join_all(futures).await;

        let mut map = HashMap::with_capacity(token_ids.len());
        for (token_id, result) in token_ids.iter().zip(results.into_iter()) {
            match result {
                Ok(price) => {
                    map.insert(token_id.clone(), price);
                }
                Err(e) => {
                    warn!(token_id, error=%e, "failed to fetch price, skipping");
                }
            }
        }
        Ok(map)
    }

    /// Fetch a public order book snapshot for one outcome token.
    #[instrument(skip(self), fields(token_id))]
    pub async fn get_order_book(&self, token_id: &str) -> Result<PublicOrderBook> {
        let url = format!("{}/book", self.base_url);
        debug!(%url, %token_id, "fetching public order book");

        let resp = self
            .http
            .get(&url)
            .query(&[("token_id", token_id)])
            .send()
            .await
            .with_context(|| format!("GET {url}?token_id={token_id} failed"))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .with_context(|| "reading book response body")?;

        if !status.is_success() {
            anyhow::bail!("CLOB /book returned {status} for {token_id}: {body}");
        }

        parse_order_book_response(&body)
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// `GET /midpoint?token_id={id}` → `{"mid": "0.553"}`.
    async fn fetch_midpoint(&self, token_id: String) -> Result<Decimal> {
        let url = format!("{}/midpoint", self.base_url);
        debug!(%token_id, "fetching midpoint");

        let resp = self
            .http
            .get(&url)
            .query(&[("token_id", &token_id)])
            .send()
            .await
            .with_context(|| format!("GET {url}?token_id={token_id} failed"))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .with_context(|| "reading midpoint response body")?;

        if !status.is_success() {
            anyhow::bail!("CLOB /midpoint returned {status} for {token_id}: {body}");
        }

        let parsed: MidpointResponse =
            serde_json::from_str(&body).with_context(|| "parsing midpoint response")?;

        Decimal::from_str(&parsed.mid)
            .with_context(|| format!("parsing midpoint value '{}' as Decimal", parsed.mid))
    }

    /// `GET /price?token_id={id}&side=buy` → `{"price": "0.55"}`.
    async fn fetch_price(&self, token_id: String) -> Result<Decimal> {
        let url = format!("{}/price", self.base_url);
        debug!(%token_id, "fetching buy price");

        let resp = self
            .http
            .get(&url)
            .query(&[("token_id", &token_id), ("side", &"buy".to_string())])
            .send()
            .await
            .with_context(|| format!("GET {url}?token_id={token_id}&side=buy failed"))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .with_context(|| "reading price response body")?;

        if !status.is_success() {
            anyhow::bail!("CLOB /price returned {status} for {token_id}: {body}");
        }

        let parsed: PriceResponse =
            serde_json::from_str(&body).with_context(|| "parsing price response")?;

        Decimal::from_str(&parsed.price)
            .with_context(|| format!("parsing price value '{}' as Decimal", parsed.price))
    }
}

impl Default for ClobClient {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_order_book_response(body: &str) -> Result<PublicOrderBook> {
    let raw: RawPublicOrderBook =
        serde_json::from_str(body).with_context(|| "parsing book response")?;

    let mut bids: Vec<PublicBookLevel> = raw
        .bids
        .into_iter()
        .map(parse_book_level)
        .collect::<Result<_>>()?;
    bids.sort_by(|left, right| right.price.cmp(&left.price));

    let mut asks: Vec<PublicBookLevel> = raw
        .asks
        .into_iter()
        .map(parse_book_level)
        .collect::<Result<_>>()?;
    asks.sort_by(|left, right| left.price.cmp(&right.price));

    Ok(PublicOrderBook {
        market: raw.market,
        asset_id: raw.asset_id,
        timestamp: raw.timestamp,
        hash: raw.hash,
        bids,
        asks,
        min_order_size: raw.min_order_size,
        tick_size: raw.tick_size,
        neg_risk: raw.neg_risk,
        last_trade_price: raw.last_trade_price,
    })
}

fn parse_book_level(raw: crate::types::RawBookLevel) -> Result<PublicBookLevel> {
    Ok(PublicBookLevel {
        price: Decimal::from_str(&raw.price)
            .with_context(|| format!("parsing book price '{}'", raw.price))?,
        size: Decimal::from_str(&raw.size)
            .with_context(|| format!("parsing book size '{}'", raw.size))?,
    })
}

#[cfg(test)]
mod tests {
    use rust_decimal_macros::dec;

    use super::parse_order_book_response;

    #[test]
    fn order_book_response_parses_and_normalizes_bid_ask_ordering() {
        let body = r#"{
          "market": "condition-1",
          "asset_id": "asset-yes",
          "timestamp": "1774417100226",
          "hash": "748febc1ad907c7c57d5f56234ac7042bc7ef348",
          "bids": [
            {"price":"0.041","size":"240"},
            {"price":"0.046","size":"60.81"},
            {"price":"0.043","size":"1100"}
          ],
          "asks": [
            {"price":"0.349","size":"100"},
            {"price":"0.111","size":"120.16"},
            {"price":"0.25","size":"400.75"}
          ],
          "min_order_size":"5",
          "tick_size":"0.001",
          "neg_risk":false,
          "last_trade_price":"0.111"
        }"#;

        let book = parse_order_book_response(body).expect("book parses");

        assert_eq!(book.asset_id, "asset-yes");
        assert_eq!(book.tick_size, dec!(0.001));
        assert_eq!(book.min_order_size, dec!(5));
        assert_eq!(book.bids[0].price, dec!(0.046));
        assert_eq!(book.bids[1].price, dec!(0.043));
        assert_eq!(book.bids[2].price, dec!(0.041));
        assert_eq!(book.asks[0].price, dec!(0.111));
        assert_eq!(book.asks[1].price, dec!(0.25));
        assert_eq!(book.asks[2].price, dec!(0.349));
    }
}
